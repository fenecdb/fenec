package com.fenecdb

/**
 * A row of an answer, a document, an object of a json field: its fields in
 * order (a [LinkedHashMap] under it), each found by name, with getters
 * that say what they expect.
 *
 * A number written without a fraction or an exponent is a [Long], the rest
 * [Double]s; a list is a [List], an object a [Row].
 */
class Row(private val fields: Map<String, Any?>) : Map<String, Any?> by fields {
    fun string(key: String): String? = fields[key] as? String

    fun long(key: String): Long? = when (val v = fields[key]) {
        is Long -> v
        is Double -> if (v == Math.floor(v) && !v.isInfinite()) v.toLong() else null
        else -> null
    }

    fun int(key: String): Int? = long(key)?.toInt()

    fun double(key: String): Double? = (fields[key] as? Number)?.toDouble()

    fun bool(key: String): Boolean? = fields[key] as? Boolean

    fun row(key: String): Row? = fields[key] as? Row

    fun list(key: String): List<Any?>? = fields[key] as? List<*>

    /** A list of numbers as a vector: each the `Float` the engine stored. */
    fun floats(key: String): FloatArray? {
        val list = list(key) ?: return null
        return FloatArray(list.size) { (list[it] as? Number)?.toFloat() ?: return null }
    }

    override fun equals(other: Any?): Boolean = other is Map<*, *> && fields == other

    override fun hashCode(): Int = fields.hashCode()

    override fun toString(): String = Json.write(this)
}

/**
 * JSON both ways: the library's answers read, parameters written. A reader
 * of its own rather than a library's -- org.json is Android's alone, and
 * Moshi or kotlinx.serialization would be a dependency of every app for a
 * page of code.
 */
internal object Json {
    fun parse(text: String): Any? {
        val r = Reader(text)
        r.space()
        val v = r.value(0)
        r.space()
        if (r.at != text.length) throw r.fail("trailing characters")
        return v
    }

    private class Reader(val s: String) {
        var at = 0

        fun fail(why: String) = FenecException(FenecException.Code.QUERY, "the library answered JSON the binding cannot read: $why at $at")

        fun space() {
            while (at < s.length && (s[at] == ' ' || s[at] == '\n' || s[at] == '\r' || s[at] == '\t')) at++
        }

        fun literal(word: String, v: Any?): Any? {
            if (!s.startsWith(word, at)) throw fail("a bad literal")
            at += word.length
            return v
        }

        fun value(depth: Int): Any? {
            if (depth > 512) throw fail("nesting too deep")
            if (at >= s.length) throw fail("the end")
            return when (s[at]) {
                'n' -> literal("null", null)
                't' -> literal("true", true)
                'f' -> literal("false", false)
                '"' -> string()
                '[' -> {
                    at++
                    val items = ArrayList<Any?>()
                    space()
                    if (at < s.length && s[at] == ']') {
                        at++
                        return items
                    }
                    while (true) {
                        space()
                        items.add(value(depth + 1))
                        space()
                        if (at >= s.length) throw fail("an unclosed list")
                        when (s[at++]) {
                            ',' -> continue
                            ']' -> return items
                            else -> throw fail("a list")
                        }
                    }
                    @Suppress("UNREACHABLE_CODE")
                    items
                }
                '{' -> {
                    at++
                    val fields = LinkedHashMap<String, Any?>()
                    space()
                    if (at < s.length && s[at] == '}') {
                        at++
                        return Row(fields)
                    }
                    while (true) {
                        space()
                        if (at >= s.length || s[at] != '"') throw fail("a key")
                        val key = string()
                        space()
                        if (at >= s.length || s[at] != ':') throw fail("a colon")
                        at++
                        space()
                        fields[key] = value(depth + 1)
                        space()
                        if (at >= s.length) throw fail("an unclosed object")
                        when (s[at++]) {
                            ',' -> continue
                            '}' -> return Row(fields)
                            else -> throw fail("an object")
                        }
                    }
                    @Suppress("UNREACHABLE_CODE")
                    null
                }
                else -> number()
            }
        }

        fun number(): Any {
            val start = at
            var whole = true
            while (at < s.length && s[at] in "0123456789+-.eE") {
                if (s[at] == '.' || s[at] == 'e' || s[at] == 'E') whole = false
                at++
            }
            val text = s.substring(start, at)
            if (text.isEmpty()) throw fail("a value")
            if (whole) text.toLongOrNull()?.let { return it }
            return text.toDoubleOrNull() ?: throw fail("a number")
        }

        fun string(): String {
            at++
            val out = StringBuilder()
            while (true) {
                if (at >= s.length) throw fail("an unclosed string")
                val c = s[at++]
                when (c) {
                    '"' -> return out.toString()
                    '\\' -> {
                        if (at >= s.length) throw fail("an escape")
                        when (val e = s[at++]) {
                            'n' -> out.append('\n')
                            't' -> out.append('\t')
                            'r' -> out.append('\r')
                            'b' -> out.append('\b')
                            'f' -> out.append('\u000c')
                            'u' -> {
                                if (at + 4 > s.length) throw fail("an escape")
                                out.append(s.substring(at, at + 4).toInt(16).toChar())
                                at += 4
                            }
                            else -> out.append(e)
                        }
                    }
                    else -> out.append(c)
                }
            }
        }
    }

    // ------------------------------------------------------------- writing

    fun write(v: Any?): String = StringBuilder().also { write(v, it) }.toString()

    fun write(v: Any?, out: StringBuilder) {
        when (v) {
            null -> out.append("null")
            is String -> quote(v, out)
            is Boolean -> out.append(v)
            is Long, is Int, is Short, is Byte -> out.append(v.toString())
            is Float -> out.append(number(v))
            is Double -> out.append(number(v))
            is Number -> out.append(number(v.toDouble()))
            is FloatArray -> list(v.size, out) { out.append(number(v[it])) }
            is DoubleArray -> list(v.size, out) { out.append(number(v[it])) }
            is IntArray -> list(v.size, out) { out.append(v[it]) }
            is LongArray -> list(v.size, out) { out.append(v[it]) }
            is List<*> -> list(v.size, out) { write(v[it], out) }
            is Array<*> -> list(v.size, out) { write(v[it], out) }
            is Map<*, *> -> {
                out.append('{')
                var first = true
                for ((k, x) in v) {
                    if (!first) out.append(',')
                    first = false
                    quote(k.toString(), out)
                    out.append(':')
                    write(x, out)
                }
                out.append('}')
            }
            else -> throw FenecException(
                FenecException.Code.BUILDER,
                "this object cannot be used as a fenecdb value: ${v::class.java.name}",
            )
        }
    }

    private inline fun list(n: Int, out: StringBuilder, item: (Int) -> Unit) {
        out.append('[')
        for (i in 0 until n) {
            if (i > 0) out.append(',')
            item(i)
        }
        out.append(']')
    }

    /**
     * A `Double` as text that reads back as it, a whole one as an integer as
     * JavaScript writes it; JSON has no word for a NaN or an infinity, and a
     * typed field refuses the `null` it goes as.
     */
    fun number(d: Double): String {
        if (d.isNaN() || d.isInfinite()) return "null"
        if (d == Math.floor(d) && Math.abs(d) < 1e15) return d.toLong().toString()
        return d.toString()
    }

    /**
     * A `Float` as the shortest text that reads back as it through a
     * `Double`, which is how the engine reads a number: one -- 7.038531e-26
     * -- has a shortest text whose `Double` lies exactly between it and the
     * next `Float` and rounds away, and goes as its `Double`'s text.
     */
    fun number(f: Float): String {
        if (f.isNaN() || f.isInfinite()) return "null"
        if (f == Math.floor(f.toDouble()).toFloat() && Math.abs(f) < 1e7f) return f.toLong().toString()
        val short = f.toString()
        return if (short.toDouble().toFloat() == f) short else f.toDouble().toString()
    }

    fun quote(s: String, out: StringBuilder) {
        out.append('"')
        for (c in s) {
            when {
                c == '"' -> out.append("\\\"")
                c == '\\' -> out.append("\\\\")
                c == '\n' -> out.append("\\n")
                c == '\r' -> out.append("\\r")
                c == '\t' -> out.append("\\t")
                c == '\b' -> out.append("\\b")
                c == '\u000c' -> out.append("\\f")
                c < ' ' -> out.append("\\u").append(String.format(java.util.Locale.ROOT, "%04x", c.code))
                else -> out.append(c)
            }
        }
        out.append('"')
    }

    /**
     * A time as JavaScript's `toISOString` writes it -- UTC, to the
     * millisecond -- which the JS builder sends a `Date` as. By hand:
     * `java.time` is on Android from API 26 only.
     */
    fun iso(ms: Long): String {
        val days = Math.floorDiv(ms, 86_400_000L)
        val rest = ms - days * 86_400_000L
        // Howard Hinnant's days-to-civil.
        val z = days + 719_468
        val era = (if (z >= 0) z else z - 146_096) / 146_097
        val doe = z - era * 146_097
        val yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365
        val doy = doe - (365 * yoe + yoe / 4 - yoe / 100)
        val mp = (5 * doy + 2) / 153
        val d = doy - (153 * mp + 2) / 5 + 1
        val m = if (mp < 10) mp + 3 else mp - 9
        val y = yoe + era * 400 + (if (m <= 2) 1 else 0)
        return String.format(
            java.util.Locale.ROOT,
            "%04d-%02d-%02dT%02d:%02d:%02d.%03dZ",
            y, m, d, rest / 3_600_000, rest / 60_000 % 60, rest / 1000 % 60, rest % 1000,
        )
    }
}
