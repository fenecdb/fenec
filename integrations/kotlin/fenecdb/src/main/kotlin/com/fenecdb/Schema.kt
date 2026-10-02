package com.fenecdb

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/**
 * A difference no open applies -- one that would lose data or could mean
 * two things -- and how to resolve it, as the engine writes them. [field]
 * is the field, or the path into a json field; null for the collection.
 */
data class SchemaRefusal(val kind: String, val collection: String, val field: String?, val message: String, val fix: String)

/**
 * What the engine found comparing the database with the schema the app
 * declares, and did: the FenecQL that adds what is missing, the migrations
 * it recorded -- [ran] false where a database made from the schema records
 * them without running them -- and what it refused.
 */
data class SchemaPlan(
    val applied: Boolean,
    val ran: Boolean,
    val migrations: List<Int>,
    val statements: List<String>,
    val refusals: List<SchemaRefusal>,
)

/** A schema the database differs from in what no open applies: [refusals] says each difference. */
class SchemaException(val refusals: List<SchemaRefusal>) :
    Exception("the database's schema differs from the app's:" + refusals.joinToString("") { "\n  - ${it.message}\n    ${it.fix}" })

/**
 * Brings the database to [fenecql] -- `create collection` and `create
 * index` statements, a `schema.fenecql` file -- as every SDK does, in the
 * engine: the [migrations] not yet recorded run first, in order, and what
 * only adds is made, all one block. Anything that would lose data or could
 * mean two things throws a [SchemaException] naming each difference, and
 * nothing is written. With `apply = false`, what an apply would do.
 */
suspend fun Fenec.schema(fenecql: String, migrations: List<String> = emptyList(), apply: Boolean = true): SchemaPlan =
    withContext(Dispatchers.IO) { schemaBlocking(fenecql, migrations, apply) }

/** [schema], on the calling thread. */
fun Fenec.schemaBlocking(fenecql: String, migrations: List<String> = emptyList(), apply: Boolean = true): SchemaPlan {
    val request = Json.write(mapOf("format" to 1L, "fenecql" to fenecql, "migrations" to migrations))
    val out = FenecNative.answer(FenecNative.schema(handle, request.encodeToByteArray(), if (apply) 1 else 0))
    lives.touch()
    val v = Json.parse(out) as Row
    val refusals = v.list("refusals").orEmpty().map {
        val r = it as Row
        SchemaRefusal(r.string("kind") ?: "", r.string("collection") ?: "", r.string("field"), r.string("message") ?: "", r.string("fix") ?: "")
    }
    if (refusals.isNotEmpty()) throw SchemaException(refusals)
    return SchemaPlan(
        v.bool("applied") ?: false,
        v.bool("ran") ?: false,
        v.list("migrations").orEmpty().map { (it as Number).toInt() },
        v.list("statements").orEmpty().map { it as String },
        emptyList(),
    )
}
