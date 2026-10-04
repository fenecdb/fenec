package com.fenecdb

// The query builder: FenecQL text and its parameters from a chain of calls.
//
//     val rows = db.from("docs")
//         .select("title")
//         .where("year", ">=", 2024)
//         .near("embed", vector, ef = 64)
//         .limit(5)
//         .rows()
//
// It makes the text web/fenec.js's builder makes of the same chain, to the
// byte, as the Python, Go, .NET, Swift and Dart builders do:
// integrations/builder-golden.json holds the chains and what each must make,
// and every builder's tests run it. Every value goes in as a parameter; a
// name -- a collection, a field, a path into a json field -- cannot, so
// names are checked against FenecQL's own rule, and that check is the
// injection boundary. A step the builder refuses throws as it is called
// (FenecException.Code.BUILDER), its message the JS builder's.

private fun refuse(message: String) = FenecException(FenecException.Code.BUILDER, message)

/**
 * A condition: what [or], [and], [not], [raw] and [cmp] make. A
 * `Map<String, Any?>` is one too, the JS builder's object condition: each
 * field's value is equality, `null` is `is null`, and a map is its operators
 * -- `mapOf("year" to mapOf("gte" to 2024))` -- in the map's order, which is
 * the text's.
 */
class Cond internal constructor(internal val node: Node) {
    companion object {
        /** Joins conditions with `or`; a map is an object condition. */
        @JvmStatic fun or(vararg conds: Any): Cond = Cond(Node.Or(conds.map(Builder::toNode)))

        /** Joins conditions with `and`: `where` already ands, so this is only needed inside [or]. */
        @JvmStatic fun and(vararg conds: Any): Cond = Cond(Node.And(conds.map(Builder::toNode)))

        /** Negates a condition. */
        @JvmStatic fun not(cond: Any): Cond = Cond(Node.Not(Builder.toNode(cond)))

        /**
         * What the builder cannot express (a function call): each `?` is
         * bound to the next parameter -- a literal `?` goes in as one too.
         * `raw("cosine(embed, ?) > ?", vector, 0.5)`
         */
        @JvmStatic fun raw(sql: String, vararg params: Any?): Cond = Cond(Node.Raw(sql, params.map(Values::normalize)))

        /** One comparison, `where`'s three arguments as a condition. */
        @JvmStatic fun cmp(field: String, op: String, value: Any?): Cond = Cond(Builder.condOf(field, op, value))

        /** The JS builder's object condition: each field held to its spec, joined with `and`. */
        @JvmStatic fun fields(fields: Map<String, Any?>): Cond = Cond(Builder.objectCond(fields))
    }
}

/**
 * A value a write works out over the row it writes, rendered as FenecQL with
 * its values as parameters: [inc] and [expr], as the JS builder's.
 */
class Computed private constructor(internal val by: Any?, internal val sql: String?, internal val params: List<Any?>) {
    companion object {
        /**
         * `mapOf("n" to Computed.inc(1))` in an update: the field plus [by],
         * counting from 0 where it is null -- `n: coalesce(n, 0) + $1` --
         * worked out under the write lock, so increments from many clients
         * all land.
         */
        @JvmStatic @JvmOverloads fun inc(by: Any? = 1): Computed {
            val finite = when (by) {
                is Double -> by.isFinite()
                is Float -> by.isFinite()
                is Number -> true
                else -> false
            }
            if (!finite) throw refuse("inc() takes a number: ${Json.write(by)}")
            return Computed(by, null, emptyList())
        }

        /**
         * A value as a FenecQL expression over the row, each `?` bound to
         * the next parameter: `Computed.expr("now()")`, `Computed.expr("price * ?", 1.2)`.
         */
        @JvmStatic fun expr(sql: String, vararg params: Any?): Computed = Computed(null, sql, params.toList())
    }
}

/** One key of a lookup's order: a field, `asc` or `desc`, and a collation (`tr` or `und`). */
data class SortKey @JvmOverloads constructor(val field: String, val direction: String = "asc", val collate: String? = null)

internal sealed class Node {
    class And(val items: List<Node>) : Node()
    class Or(val items: List<Node>) : Node()
    class Not(val item: Node) : Node()
    class Null(val field: String, val negated: Boolean) : Node()
    class In(val field: String, val values: List<Any?>) : Node()
    class Cmp(val field: String, val op: String, val value: Any?) : Node()
    class Raw(val sql: String, val params: List<Any?>) : Node()
}

internal object Builder {
    const val MAX_LOOKUP_DEPTH = 8

    val ops = mapOf(
        "=" to "=", "eq" to "=",
        "!=" to "!=", "ne" to "!=", "neq" to "!=",
        "<" to "<", "lt" to "<",
        "<=" to "<=", "lte" to "<=", "le" to "<=",
        ">" to ">", "gt" to ">",
        ">=" to ">=", "gte" to ">=", "ge" to ">=",
        "~" to "~", "like" to "~", "contains" to "~",
        "has" to "has",
        "in" to "in",
    )

    /**
     * FenecQL's identifier: a Unicode letter or `_`, then letters, digits and
     * `_`, as the lexer reads it -- JavaScript's `\p{Alphabetic}` and `\p{N}`,
     * which `Character.isAlphabetic` and the number categories name the same.
     */
    fun isIdent(s: String): Boolean {
        if (s.isEmpty()) return false
        var i = 0
        var first = true
        while (i < s.length) {
            val cp = s.codePointAt(i)
            val start = cp == '_'.code || Character.isAlphabetic(cp)
            val more = when (Character.getType(cp).toByte()) {
                Character.DECIMAL_DIGIT_NUMBER, Character.LETTER_NUMBER, Character.OTHER_NUMBER -> true
                else -> false
            }
            if (!(start || (!first && more))) return false
            first = false
            i += Character.charCount(cp)
        }
        return true
    }

    fun ident(name: String, what: String = "field"): String =
        if (isIdent(name)) name else throw refuse("invalid $what name: ${quote(name)}")

    /** A field's name, or a path into a json field: `meta.lang`. */
    fun path(name: String): String =
        if (name.split('.').all(::isIdent)) name else throw refuse("invalid field name: ${quote(name)}")

    /** A name as `JSON.stringify` writes it, which the messages quote with. */
    fun quote(s: String): String = StringBuilder().also { Json.quote(s, it) }.toString()

    /** What JavaScript's `String.prototype.trim` takes off. */
    private fun jsSpace(c: Char): Boolean = c in "\t\n\u000b\u000c\r       　﻿" ||
        c in ' '..' '

    private fun asciiIdent(s: String): Boolean =
        s.isNotEmpty() && (s[0] in 'a'..'z' || s[0] in 'A'..'Z' || s[0] == '_') &&
            s.all { it in 'a'..'z' || it in 'A'..'Z' || it in '0'..'9' || it == '_' }

    /**
     * A select item: a field, or an aggregate spelled as FenecQL spells it --
     * `count(*)`, `sum(total)`, `avg(f)`, `min(f)`, `max(f)` -- answering
     * under that name. Read by hand rather than by a case-blind pattern,
     * which folds the Kelvin sign onto `k` where the JS builder's does not.
     */
    fun column(name: String): Pair<String, Boolean> {
        val s = name.trim(::jsSpace)
        val open = s.indexOf('(')
        if (open > 0 && s.endsWith(")")) {
            val fn = s.substring(0, open)
            val arg = s.substring(open + 1, s.length - 1)
            val low = if (fn.all { it in 'a'..'z' || it in 'A'..'Z' }) fn.lowercase() else null
            if (low == "count" && (arg.isEmpty() || arg == "*")) return "count(*)" to true
            if (low in setOf("sum", "avg", "min", "max") && asciiIdent(arg)) return "$low($arg)" to true
        }
        return path(name) to false
    }

    fun direction(dir: String): Boolean = when (dir.lowercase()) {
        "asc" -> true
        "desc" -> false
        else -> throw refuse("order direction must be 'asc' or 'desc': $dir")
    }

    /** The collations the engine knows; the name is spliced into the text, so it is checked against the list. */
    fun collation(name: String?): String? = when (name) {
        null, "und", "tr" -> name
        else -> throw refuse("unknown collation: ${quote(name)}; there are 'und' and 'tr'")
    }

    /** `limit`, `offset`, `ef` and the rest are literals, never parameters: a whole number JavaScript holds exactly. */
    /** A tag or an ellipsis: text, or refused naming the value as `JSON.stringify` writes it. */
    fun text(v: Any, what: String): String =
        v as? String ?: throw refuse("$what must be text: ${Json.write(Values.normalize(v))}")

    fun whole(n: Long, what: String): Long =
        if (n in 0..(1L shl 53) - 1) n else throw refuse("$what must be a non-negative integer: $n")

    fun toNode(c: Any): Node = when (c) {
        is Cond -> c.node
        is Map<*, *> -> objectCond(c.entries.associate { it.key.toString() to it.value })
        else -> throw refuse("expected an object as a condition: ${Json.write(Values.normalize(c))}")
    }

    fun objectCond(fields: Map<String, Any?>): Node {
        val items = fields.map { (k, v) -> fieldCond(path(k), v) }
        return if (items.size == 1) items[0] else Node.And(items)
    }

    fun condOf(field: String, op: String, value: Any?): Node {
        val o = ops[op] ?: throw refuse("unknown operator `$op`")
        val f = path(field)
        return if (o == "in") inCond(f, value) else cmp(f, o, value)
    }

    /** One field's condition: a value is equality, null is null, a map its operators. */
    fun fieldCond(field: String, spec: Any?): Node {
        if (spec == null) return Node.Null(field, false)
        if (spec !is Map<*, *>) return cmp(field, "=", spec)
        val items = ArrayList<Node>()
        for ((key, v) in spec) {
            val k = key.toString()
            if (k == "not") {
                items.add(if (v == null) Node.Null(field, true) else Node.Not(fieldCond(field, v)))
                continue
            }
            val op = ops[k] ?: throw refuse("unknown operator `$k` (field: $field)")
            items.add(if (op == "in") inCond(field, v) else cmp(field, op, v))
        }
        return when (items.size) {
            0 -> throw refuse("empty condition object (field: $field)")
            1 -> items[0]
            else -> Node.And(items)
        }
    }

    private fun inCond(field: String, values: Any?): Node {
        val list = when (values) {
            is List<*> -> values
            is Array<*> -> values.toList()
            else -> throw refuse("`in` expects an array (field: $field)")
        }
        if (list.isEmpty()) throw refuse("`in` does not accept an empty array (field: $field)")
        return Node.In(field, list)
    }

    /** `= null` is never true in FenecQL; what is meant is `is null`. */
    private fun cmp(field: String, op: String, value: Any?): Node = when {
        value != null -> Node.Cmp(field, op, value)
        op == "=" -> Node.Null(field, false)
        op == "!=" -> Node.Null(field, true)
        else -> throw refuse("`$op` cannot be used with null (field: $field)")
    }

    /** Flattens empty and single-child junctions before rendering: the parentheses depend on the child count. */
    fun prune(c: Node): Node? = when (c) {
        is Node.And -> c.items.mapNotNull(::prune).let { if (it.isEmpty()) null else if (it.size == 1) it[0] else Node.And(it) }
        is Node.Or -> c.items.mapNotNull(::prune).let { if (it.isEmpty()) null else if (it.size == 1) it[0] else Node.Or(it) }
        is Node.Not -> prune(c.item)?.let { Node.Not(it) }
        else -> c
    }

    fun render(c: Node, bind: Binder, parent: String? = null): String = when (c) {
        is Node.And, is Node.Or -> {
            val word = if (c is Node.And) "and" else "or"
            val items = if (c is Node.And) c.items else (c as Node.Or).items
            val s = items.joinToString(" $word ") { render(it, bind, word) }
            // `and` binds tighter than `or`: one inside the other needs parens.
            if (parent != null && parent != word) "($s)" else s
        }
        is Node.Not -> "not (${render(c.item, bind)})"
        is Node.Null -> "${c.field} is ${if (c.negated) "not " else ""}null"
        is Node.In -> "${c.field} in [${c.values.joinToString(", ") { bind.bind(it) }}]"
        is Node.Cmp -> "${c.field} ${c.op} ${bind.bind(c.value)}"
        is Node.Raw -> {
            val pieces = c.sql.split('?')
            val out = StringBuilder()
            for (i in 0 until pieces.size - 1) {
                if (i >= c.params.size) throw refuse("raw(): more `?` placeholders than parameters")
                out.append(pieces[i]).append(bind.bind(c.params[i]))
            }
            if (pieces.size - 1 != c.params.size) throw refuse("raw(): too many parameters given")
            out.append(pieces.last()).toString()
        }
    }

    fun renderDoc(doc: Any?, bind: Binder, insert: Boolean = false): String {
        if (doc !is Map<*, *>) throw refuse("expected a document object")
        if (doc.isEmpty()) throw refuse("cannot write an empty document")
        return "{" + doc.entries.joinToString(", ") { (k, v) ->
            val key = path(k.toString())
            "$key: ${value(key, v, bind, insert)}"
        } + "}"
    }

    /** A document's value: [Computed]'s text, or a parameter. */
    private fun value(key: String, v: Any?, bind: Binder, insert: Boolean): String {
        if (v !is Computed) return bind.bind(v)
        val sql = v.sql
        if (sql == null) {
            if (insert) throw refuse("inc() reads the row it changes: use it in update (field: $key)")
            return "coalesce($key, 0) + ${bind.bind(v.by)}"
        }
        val pieces = sql.split('?')
        val out = StringBuilder()
        for (i in 0 until pieces.size - 1) {
            if (i >= v.params.size) throw refuse("expr(): more `?` placeholders than parameters")
            out.append(pieces[i]).append(bind.bind(v.params[i]))
        }
        if (pieces.size - 1 != v.params.size) throw refuse("expr(): too many parameters given")
        return out.append(pieces.last()).toString()
    }

    /** Whether a `raw` fragment may read a collection of its own. */
    fun readsMore(c: Node): Boolean = when (c) {
        is Node.And -> c.items.any(::readsMore)
        is Node.Or -> c.items.any(::readsMore)
        is Node.Not -> readsMore(c.item)
        is Node.Raw -> Regex("\\bget\\b", RegexOption.IGNORE_CASE).containsMatchIn(c.sql)
        else -> false
    }
}

internal class Binder {
    val params = ArrayList<Any?>()

    fun bind(v: Any?): String {
        params.add(Values.normalize(v))
        return "$${params.size}"
    }
}

/** A statement and its parameters, as they would be run. */
data class Statement(val text: String, val params: List<Any?>)

/**
 * A query over one collection, made by [Fenec.from] or [Query.from].
 * Immutable: each call hands back a new one, so a base query can be kept and
 * branched from, from several threads too.
 */
class Query private constructor(private val s: State) {
    private class VectorClause(val field: String, val vector: Any?, val n: Long?, val exact: Boolean)

    private class Key(val field: String, val asc: Boolean, val collate: String?)

    /** A highlight (no [words]) or a snippet of a field, in the select list. */
    private class Mark(val field: String, val words: Long?, val ellipsis: String?, val pre: String?, val post: String?) {
        val kind get() = if (words == null) "highlight" else "snippet"
    }

    private class Facet(val field: String, val top: Long?)

    private class Level(
        val collection: String, val on: String, val parent: String?, val project: List<String>?, val cond: List<Node>,
        val required: Boolean, val order: List<Key>, val limit: Long?, val offset: Long,
    )

    private data class State(
        val collection: String,
        val exec: (suspend (String, List<Any?>) -> Answer)? = null,
        val project: List<String>? = null,
        val aggregate: Boolean = false,
        val group: String? = null,
        val cond: List<Node> = emptyList(),
        val near: VectorClause? = null,
        val match: Pair<String, String>? = null,
        val rerank: VectorClause? = null,
        val fuse: Pair<Long?, Long?>? = null,
        val order: List<Key> = emptyList(),
        val limit: Long? = null,
        val offset: Long = 0,
        val count: Boolean = false,
        val lookups: List<Level> = emptyList(),
        val marks: List<Mark> = emptyList(),
        val facets: List<Facet> = emptyList(),
    )

    companion object {
        /** A query bound to no database: for its text alone, [toFenecQL]. */
        @JvmStatic fun from(collection: String): Query = Query(State(Builder.ident(collection, "collection")))
    }

    /** The collection the query is over. */
    val collection: String get() = s.collection

    /** The query bound to whatever runs a text and its parameters. */
    fun bind(exec: suspend (String, List<Any?>) -> Answer): Query = Query(s.copy(exec = exec))

    /**
     * Every collection the query reads -- its own and each lookup's -- or
     * `null` when a `raw` fragment may read one more: a live query runs again
     * when one of them is written.
     */
    val reads: List<String>?
        get() {
            if (s.cond.any(Builder::readsMore) || s.lookups.any { l -> l.cond.any(Builder::readsMore) }) return null
            return (listOf(s.collection) + s.lookups.map { it.collection }).distinct()
        }

    /**
     * `select a, b`; none, or `"*"`, is every field. Aggregates go in the same
     * list as FenecQL spells them and answer under that name:
     * `select("status", "count(*)", "sum(total)").group("status")`.
     */
    fun select(vararg columns: String): Query = select(columns.toList())

    fun select(columns: List<String>): Query {
        if (columns.isEmpty() || "*" in columns) return Query(s.copy(project = null, aggregate = false))
        val cs = columns.map(Builder::column)
        return Query(s.copy(project = cs.map { it.first }, aggregate = cs.any { it.second }))
    }

    /** `group field`: a row per value, for a select list that aggregates. */
    fun group(field: String): Query = Query(s.copy(group = Builder.ident(field)))

    /**
     * `field op value`, joined to the conditions before with `and`. The op is
     * a symbol or its word: `=`, `!=`, `<`, `<=`, `>`, `>=`, `~`, `has`, `in`
     * (a list), or `eq`, `ne`, `lt`, `gte`, `like`, `contains` ... A null with
     * `=` or `!=` is `is null` and `is not null`.
     */
    fun where(field: String, op: String, value: Any?): Query = and(Builder.condOf(field, op, value))

    /** The field held to a spec: a value is equality, null is null, a map its operators. */
    fun where(field: String, spec: Any?): Query = and(Builder.fieldCond(Builder.path(field), spec))

    /** A condition [Cond] made, or a map of fields. */
    fun where(cond: Any): Query = and(Builder.toNode(cond))

    /** Everything conditioned so far, or `field op value`. */
    fun orWhere(field: String, op: String, value: Any?): Query = or(Builder.condOf(field, op, value))

    /** Everything conditioned so far, or the field held to a spec. */
    fun orWhere(field: String, spec: Any?): Query = or(Builder.fieldCond(Builder.path(field), spec))

    /** Everything conditioned so far, or the condition. */
    fun orWhere(cond: Any): Query = or(Builder.toNode(cond))

    private fun and(c: Node) = Query(s.copy(cond = s.cond + c))

    private fun or(right: Node) =
        Query(s.copy(cond = if (s.cond.isEmpty()) listOf(right) else listOf(Node.Or(listOf(Node.And(s.cond), right)))))

    /** `near field $n [ef N] [exact]`. A [FloatArray] goes to the library as its bytes. */
    @JvmOverloads
    fun near(field: String, vector: Any?, ef: Long? = null, exact: Boolean = false): Query =
        Query(s.copy(near = VectorClause(Builder.ident(field), vector, ef?.let { Builder.whole(it, "ef") }, exact)))

    /** `match field $n`: BM25 over a `@text` index. */
    fun match(field: String, query: String): Query = Query(s.copy(match = Builder.ident(field) to query))

    /** `fuse [k N] [candidates N]`: with both [match] and [near], ranks by both. */
    @JvmOverloads
    fun fuse(k: Long? = null, candidates: Long? = null): Query =
        Query(s.copy(fuse = k?.let { Builder.whole(it, "k") } to candidates?.let { Builder.whole(it, "candidates") }))

    /** `rerank field $n [candidates N]`: reorders what [match] found by exact distance. */
    @JvmOverloads
    fun rerank(field: String, vector: Any?, candidates: Long? = null): Query =
        Query(s.copy(rerank = VectorClause(Builder.ident(field), vector, candidates?.let { Builder.whole(it, "candidates") }, false)))

    /**
     * `lookup name on child [= parent] ...`: each row's children, attached to
     * it. [on] names the child's field, [parentKey] the parent's (`id` unless
     * given); the rest binds to the looked-up collection, and [limit] counts
     * children per parent. [required] drops a parent no child matches.
     * Called again, it chains onto the collection the call before named.
     */
    @JvmOverloads
    fun lookup(
        name: String, on: String?, parentKey: String? = null, select: List<String>? = null, where: Any? = null,
        required: Boolean = false, order: List<SortKey> = emptyList(), limit: Long? = null, offset: Long? = null,
    ): Query {
        if (on.isNullOrEmpty()) throw refuse("lookup needs `on`: the child field holding the key")
        val level = Level(
            collection = Builder.ident(name, "collection"),
            on = Builder.ident(on),
            parent = parentKey?.let { Builder.ident(it) },
            project = if (select == null || "*" in select) null else select.map(Builder::path),
            cond = if (where == null) emptyList() else listOf(Builder.toNode(where)),
            required = required,
            order = order.map { Key(Builder.path(it.field), Builder.direction(it.direction), Builder.collation(it.collate)) },
            limit = limit?.let { Builder.whole(it, "limit") },
            offset = offset?.let { Builder.whole(it, "offset") } ?: 0,
        )
        return Query(s.copy(lookups = s.lookups + level))
    }

    /**
     * `highlight(field)` in the select list: where the terms [match] found
     * stand in the field's text, `[[start, end], ...]` in UTF-16 offsets --
     * a [String]'s own -- or, given [pre] and [post], the text with each
     * marked. The row answers it under `highlight(field)`, after the fields
     * [select] named.
     */
    @JvmOverloads
    fun highlight(field: String, pre: String? = null, post: String? = null): Query = mark(field, null, null, pre, post)

    /**
     * `snippet(field, words)`: the window of [words] words around the
     * densest marks, `{"text": ..., "marks": [[start, end], ...]}` -- or the
     * marked text, given [pre] and [post] -- with [ellipsis] where it was
     * cut. The row answers it under `snippet(field)`.
     */
    @JvmOverloads
    fun snippet(field: String, words: Long, ellipsis: String? = null, pre: String? = null, post: String? = null): Query =
        mark(field, words, ellipsis, pre, post)

    /**
     * Both marks, the tags and the ellipsis taken as any value: the JS
     * builder checks they are text, and the golden file holds that check,
     * which the typed calls above can never fail.
     */
    internal fun mark(field: String, words: Long?, ellipsis: Any?, pre: Any?, post: Any?): Query {
        val f = Builder.ident(field)
        val kind = if (words == null) "highlight" else "snippet"
        // In the JS builder's order, so a chain wrong twice is refused for the same thing.
        val n = words?.let { Builder.whole(it, "snippet words") }
        if ((pre == null) != (post == null)) throw refuse("$kind takes both pre and post, or neither")
        val m = Mark(
            f,
            n,
            null,
            pre?.let { Builder.text(it, "$kind pre") },
            post?.let { Builder.text(it, "$kind post") },
        )
        if (n == 0L) throw refuse("snippet shows at least one word")
        val mark = if (ellipsis == null) m else Mark(f, n, Builder.text(ellipsis, "snippet ellipsis"), m.pre, m.post)
        if (s.marks.any { it.kind == kind && it.field == f }) throw refuse("$kind($f) is asked twice")
        return Query(s.copy(marks = s.marks + mark))
    }

    /**
     * `facet field [top N]`: each value the field -- or a path into a json
     * field -- holds over every row the query matches, not the page alone,
     * and how many hold it, most first. The counts come back beside the
     * rows: [rows]'s [Rows.facets], and a live query's.
     */
    @JvmOverloads
    fun facet(field: String, top: Long? = null): Query {
        val f = Facet(Builder.path(field), top?.let { Builder.whole(it, "facet top") })
        if (f.top == 0L) throw refuse("facet ${f.field} top 0 answers nothing")
        if (s.facets.any { it.field == f.field }) throw refuse("facet ${f.field} is asked twice")
        return Query(s.copy(facets = s.facets + f))
    }

    /** `order field asc|desc`: each call adds a key. `collate = "tr"` puts text in Turkish order. */
    @JvmOverloads
    fun order(field: String, direction: String = "asc", collate: String? = null): Query =
        // Over groups a key may be an aggregate of the list, by its name.
        Query(s.copy(order = s.order + Key(Builder.column(field).first, Builder.direction(direction), Builder.collation(collate))))

    /** How many rows come back. */
    fun limit(n: Long): Query = Query(s.copy(limit = Builder.whole(n, "limit")))

    /** How many rows are passed over first. */
    fun offset(n: Long): Query = Query(s.copy(offset = Builder.whole(n, "offset")))

    // ---------------------------------------------------------------- the text

    /** The statement and its parameters, as they would be run. */
    fun toFenecQL(): Statement {
        val bind = Binder()
        return Statement(text(bind), bind.params)
    }

    private fun text(bind: Binder): String {
        // The engine refuses each of these too; failing here runs nothing.
        if (s.group != null && !s.aggregate) throw refuse("group ${s.group} needs an aggregate in select: 'count(*)'")
        if (s.aggregate) {
            val clash = when {
                s.near != null -> "near"
                s.match != null -> "match"
                s.lookups.isNotEmpty() -> "lookup"
                s.count -> "count"
                else -> null
            }
            if (clash != null) throw refuse("aggregates cannot be combined with $clash")
            if (s.group == null && (s.order.isNotEmpty() || s.limit != null || s.offset > 0)) {
                throw refuse("aggregates answer one row; group makes a row per value")
            }
        }
        if (s.marks.isNotEmpty()) {
            val what = s.marks[0].kind
            if (s.match == null) throw refuse("$what needs match: it marks the terms match found")
            if (s.aggregate) throw refuse("$what marks a row's text; aggregates answer groups")
        }
        if (s.facets.isNotEmpty()) {
            if (s.near != null) {
                throw refuse("facet counts the rows a filter or match selects, and near ranks every row: ask the facets without near")
            }
            if (s.aggregate) throw refuse("facet cannot be combined with aggregates: group counts by value")
        }
        if (s.rerank != null && s.match == null) throw refuse("rerank needs match: it reorders what match found")
        if (s.match != null && s.near != null && s.fuse == null) {
            throw refuse("match and near cannot be combined: both order the result; fuse() ranks by both")
        }
        if (s.fuse != null && (s.match == null || s.near == null)) throw refuse("fuse combines match and near: the query needs both")
        if (s.fuse != null && s.rerank != null) throw refuse("fuse and rerank are two ways to use a vector with match: pick one")
        if (s.lookups.isNotEmpty()) {
            val clash = when {
                s.near != null -> "near"
                s.match != null -> "match"
                s.rerank != null -> "rerank"
                else -> null
            }
            if (clash != null) throw refuse("lookup cannot be combined with $clash")
            if (s.count && !s.lookups[0].required) {
                throw refuse("count cannot be used with lookup unless it is required: there is nothing to attach children to")
            }
            if (s.lookups.size > Builder.MAX_LOOKUP_DEPTH) {
                throw refuse("lookup chained too deep: at most ${Builder.MAX_LOOKUP_DEPTH} levels")
            }
            val seen = mutableListOf(s.collection)
            for (l in s.lookups) {
                if (l.collection in seen) throw refuse("${l.collection} cannot look itself up: both sides would answer to the same name")
                seen.add(l.collection)
            }
        }
        if (s.count) extraClause()?.let { throw refuse("count cannot be used with `$it`") }

        val sql = StringBuilder("get ${s.collection}")
        // The marks after the fields `select` named, or after every field;
        // their tags bound in the order they stand.
        val items = s.marks.map { m ->
            val item = StringBuilder("${m.kind}(${m.field}")
            m.words?.let { item.append(", ").append(it) }
            // A snippet's tags come after its ellipsis, so tags alone bind an empty one.
            if (m.ellipsis != null || (m.words != null && m.pre != null)) item.append(", ").append(bind.bind(m.ellipsis ?: ""))
            if (m.pre != null) item.append(", ").append(bind.bind(m.pre)).append(", ").append(bind.bind(m.post))
            item.append(")").toString()
        }
        if (s.project != null || items.isNotEmpty()) {
            sql.append(" select ").append(((s.project ?: listOf("*")) + items).joinToString(", "))
        }
        whereOf(s.cond, bind)?.let { sql.append(" where ").append(it) }
        s.group?.let { sql.append(" group ").append(it) }
        s.near?.let { n ->
            sql.append(" near ${n.field} ${bind.bind(n.vector)}")
            n.n?.let { sql.append(" ef $it") }
            if (n.exact) sql.append(" exact")
        }
        s.match?.let { (f, q) -> sql.append(" match $f ${bind.bind(q)}") }
        s.rerank?.let { r ->
            sql.append(" rerank ${r.field} ${bind.bind(r.vector)}")
            r.n?.let { sql.append(" candidates $it") }
        }
        s.fuse?.let { (k, c) ->
            sql.append(" fuse")
            k?.let { sql.append(" k $it") }
            c?.let { sql.append(" candidates $it") }
        }
        appendOrder(sql, s.order)
        s.limit?.let { sql.append(" limit $it") }
        if (s.offset > 0) sql.append(" offset ${s.offset}")
        if (s.count) sql.append(" count")
        if (s.facets.isNotEmpty()) {
            sql.append(" facet ").append(s.facets.joinToString(", ") { f -> f.top?.let { "${f.field} top $it" } ?: f.field })
        }
        // Terminal, so every clause after it is the child's -- and last, so
        // its parameters come after the parent's.
        for (l in s.lookups) {
            sql.append(" lookup ${l.collection} on ${l.on}")
            l.parent?.let { sql.append(" = $it") }
            if (l.required) sql.append(" required")
            l.project?.let { sql.append(" select ").append(it.joinToString(", ")) }
            whereOf(l.cond, bind)?.let { sql.append(" where ").append(it) }
            appendOrder(sql, l.order)
            l.limit?.let { sql.append(" limit $it") }
            if (l.offset > 0) sql.append(" offset ${l.offset}")
        }
        return sql.toString()
    }

    private fun appendOrder(sql: StringBuilder, keys: List<Key>) {
        keys.forEachIndexed { i, k ->
            sql.append(if (i == 0) " order " else ", ").append(k.field)
            k.collate?.let { sql.append(" collate ").append(it) }
            sql.append(if (k.asc) " asc" else " desc")
        }
    }

    private fun whereOf(cond: List<Node>, bind: Binder): String? = Builder.prune(Node.And(cond))?.let { Builder.render(it, bind) }

    private fun extraClause(): String? = when {
        s.near != null -> "near"
        s.match != null -> "match"
        s.rerank != null -> "rerank"
        s.order.isNotEmpty() -> "order"
        s.limit != null -> "limit"
        s.offset > 0 -> "offset"
        s.project != null -> "select"
        else -> null
    }

    // Near, order, limit mean something only to a read; dropped from a write,
    // limit(1).delete() would delete every row.
    private fun assertPlain(verb: String) {
        extraClause()?.let { throw refuse("$verb cannot be used with `$it`") }
        if (s.lookups.isNotEmpty()) throw refuse("$verb cannot be used with `lookup`")
        if (s.facets.isNotEmpty()) throw refuse("$verb cannot be used with `facet`")
        if (verb == "insert" && s.cond.isNotEmpty()) throw refuse("insert cannot be used with `where`")
    }

    // An update or delete of every row is too easy to do by accident and
    // cannot be undone: it has to be asked for, with all.
    private fun requireFilter(verb: String, all: Boolean, bind: Binder): String {
        whereOf(s.cond, bind)?.let { return " where $it" }
        if (all) return ""
        throw refuse("an unfiltered $verb covers the whole collection; if you mean it, $verb({ all: true })")
    }

    private fun docsOf(docs: Any?): List<Any?> = when (docs) {
        is List<*> -> docs
        is Array<*> -> docs.toList()
        else -> listOf(docs)
    }

    /**
     * The `put` of a document -- a map of fields, in its order -- or a list of
     * them, not run. [ifAbsent]: `put ... if absent`, which passes over a
     * document whose id or `@unique` value a row holds and counts only what
     * it wrote -- a lock taken, or not, in one statement.
     */
    @JvmOverloads
    fun toInsert(docs: Any?, ifAbsent: Boolean = false): Statement {
        assertPlain("insert")
        val list = docsOf(docs)
        if (list.isEmpty()) throw refuse("cannot write an empty document list")
        val bind = Binder()
        val body = list.joinToString(", ") { Builder.renderDoc(it, bind, insert = true) }
        val absent = if (ifAbsent) " if absent" else ""
        return Statement("put ${s.collection} ${if (list.size == 1) body else "[$body]"}$absent", bind.params)
    }

    /** The `set` of the rows the filter names, not run; with no filter it is refused unless [all]. */
    @JvmOverloads
    fun toUpdate(patch: Any?, all: Boolean = false): Statement {
        assertPlain("update")
        val bind = Binder()
        val body = Builder.renderDoc(patch, bind)
        return Statement("set ${s.collection} $body${requireFilter("update", all, bind)}", bind.params)
    }

    /** The `del` of the rows the filter names, not run; with no filter it is refused unless [all]. */
    @JvmOverloads
    fun toDelete(all: Boolean = false): Statement {
        assertPlain("delete")
        val bind = Binder()
        return Statement("del ${s.collection}${requireFilter("delete", all, bind)}", bind.params)
    }

    // ---------------------------------------------------------------- running

    private suspend fun run(st: Statement): Answer {
        val exec = s.exec ?: throw refuse("query is not bound to a connection: use db.from(...) (toFenecQL() if you only want the text)")
        return exec(st.text, st.params)
    }

    /** Runs the query and hands back its rows, what [facet] counted as their [Rows.facets]. */
    suspend fun rows(): Rows = run(toFenecQL()).page

    /**
     * Runs the query and hands back its whole answer: the rows, and what
     * [facet] counted beside them as [Answer.facets].
     */
    suspend fun answer(): Answer = run(toFenecQL())

    /** The first row with `limit 1`, or null. */
    suspend fun first(): Row? = limit(1).rows().firstOrNull()

    /** How many rows match: `get ... count`, no row decoded. */
    suspend fun count(): Long = Query(s.copy(count = true)).rows().firstOrNull()?.long("count") ?: 0

    /** The path the query took, a line a step; the query runs to tell. */
    suspend fun explain(): List<String> {
        val st = toFenecQL()
        return run(Statement("explain ${st.text}", st.params)).rows.map { it.string("plan") ?: "" }
    }

    /**
     * Puts a document -- a map of fields -- or a list of them: how many it
     * wrote, which with [ifAbsent] leaves out those already held. None is no
     * statement.
     */
    suspend fun insert(docs: Any?, ifAbsent: Boolean = false): Long {
        if (docsOf(docs).isEmpty()) return 0
        return run(toInsert(docs, ifAbsent)).affected
    }

    /** Sets the patch's fields on the rows the filter names; with no filter it is refused unless [all]. */
    suspend fun update(patch: Any?, all: Boolean = false): Long = run(toUpdate(patch, all)).affected

    /** Deletes the rows the filter names; with no filter it is refused unless [all]. */
    suspend fun delete(all: Boolean = false): Long = run(toDelete(all)).affected

    /** [rows] on the calling thread, for Java. */
    fun rowsBlocking(): Rows = kotlinx.coroutines.runBlocking { rows() }

    /** [answer] on the calling thread, for Java. */
    fun answerBlocking(): Answer = kotlinx.coroutines.runBlocking { answer() }

    /** [count] on the calling thread, for Java. */
    fun countBlocking(): Long = kotlinx.coroutines.runBlocking { count() }

    /** [insert] on the calling thread, for Java. */
    @JvmOverloads
    fun insertBlocking(docs: Any?, ifAbsent: Boolean = false): Long = kotlinx.coroutines.runBlocking { insert(docs, ifAbsent) }

    /** [update] on the calling thread, for Java. */
    @JvmOverloads
    fun updateBlocking(patch: Any?, all: Boolean = false): Long = kotlinx.coroutines.runBlocking { update(patch, all) }

    /** [delete] on the calling thread, for Java. */
    @JvmOverloads
    fun deleteBlocking(all: Boolean = false): Long = kotlinx.coroutines.runBlocking { delete(all) }
}
