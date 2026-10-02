package com.fenecdb

import kotlinx.coroutines.runBlocking
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

/** A schema written as FenecQL, through the native library: made, checked again, refused when it would lose data, and migrated once. */
class SchemaTest {
    @Test
    fun aSchemaIsMadeCheckedAndMigrated() = runBlocking {
        val file = scratch()
        val v1 = "create collection notes (title text required, at timestamp @sorted)"

        val db = Fenec.open(file)
        val made = db.schema(v1)
        assertTrue(made.applied)
        assertEquals(listOf(v1), made.statements)
        db.execute("put notes {title: \"a\"}")
        db.close()

        // Opened again with the same schema: nothing to do.
        val again = Fenec.open(file)
        assertEquals(SchemaPlan(false, false, emptyList(), emptyList(), emptyList()), again.schema(v1))
        // A rename unsaid could be a drop and an add: refused, nothing written.
        val v2 = "create collection notes (name text required, at timestamp @sorted)"
        val e = assertFailsWith<SchemaException> { again.schema(v2) }
        assertTrue(e.refusals.any { it.kind == "field_not_declared" && it.field == "title" })
        // Said, as a migration: run once, recorded.
        val moved = listOf("alter collection notes rename field title to name")
        val migrated = again.schema(v2, moved)
        assertTrue(migrated.ran)
        assertEquals(listOf(1), migrated.migrations)
        assertEquals(listOf("a"), again.query("get notes select name").map { it.string("name") })
        assertEquals(emptyList(), again.schema(v2, moved).migrations)
        // A plan writes nothing.
        val plan = again.schemaBlocking("$v2\ncreate collection tags (name text)", moved, apply = false)
        assertEquals(listOf("create collection tags (name text)"), plan.statements)
        assertTrue(!plan.applied)
        again.close()
    }

    /** What `fenec types --lang kotlin` writes reads a row the binding answers. */
    @Test
    fun generatedTypesReadARow() = runBlocking {
        val db = Fenec.memory()
        db.schema("create collection product_reviews (product_id int required @hash, stars int, notes text collate und)")
        db.execute("put product_reviews {product_id: 7, notes: \"fine\"}")
        val r = fenecschema.ProductReviews.from(db.query("get product_reviews")[0])
        assertEquals(fenecschema.ProductReviews(1, 7, null, "fine"), r)
        db.close()
    }
}
