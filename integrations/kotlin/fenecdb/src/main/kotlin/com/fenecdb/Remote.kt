package com.fenecdb

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL

/**
 * A fenec-server over HTTP, with no file on the device: every query goes to
 * the server (`POST /query`), through the same builder. What
 * [Fenec.Companion.connect] makes, as `connect` does in JS.
 *
 * ```kotlin
 * val db = Fenec.connect("https://api.example.com", token = jwt)
 * val open = db.from("todos").where("done", false).rows()
 * ```
 *
 * The requests are `HttpURLConnection`'s, so TLS is the system's.
 */
class FenecRemote internal constructor(url: String, @Volatile private var token: String?) {
    val url: String = url.trimEnd('/')

    /** A fresh token for the requests from here on. */
    fun setToken(token: String?) {
        this.token = token
    }

    /** Runs FenecQL on the server with [params] for `$1`, `$2` ... */
    suspend fun run(text: String, vararg params: Any?): Answer = runList(text, params.toList())

    suspend fun runList(text: String, params: List<Any?>): Answer = withContext(Dispatchers.IO) { send(text, params) }

    /** The rows a statement answers. */
    suspend fun query(text: String, vararg params: Any?): List<Row> = runList(text, params.toList()).rows

    /** A write: how many documents it wrote. */
    suspend fun execute(text: String, vararg params: Any?): Long = runList(text, params.toList()).affected

    /** The query builder, its queries run on the server. */
    fun from(collection: String): Query = Query.from(collection).bind { text, params -> runList(text, params) }

    /** [run], on the calling thread. */
    fun runBlocking(text: String, vararg params: Any?): Answer = send(text, params.toList())

    private fun send(text: String, params: List<Any?>): Answer {
        val c = URL("$url/query").openConnection() as HttpURLConnection
        c.requestMethod = "POST"
        c.connectTimeout = 10_000
        c.readTimeout = 60_000
        c.setRequestProperty("content-type", "application/json")
        token?.let { c.setRequestProperty("authorization", "Bearer $it") }
        c.doOutput = true
        val (status, body) = try {
            val values = params.map { Values.normalize(it) }
            c.outputStream.use { it.write(Json.write(mapOf("query" to text, "params" to values)).encodeToByteArray()) }
            val status = c.responseCode
            status to ((if (status >= 400) c.errorStream else c.inputStream)?.use { String(it.readBytes(), Charsets.UTF_8) } ?: "")
        } catch (e: IOException) {
            throw FenecException(FenecException.Code.IO, "the server could not be reached: ${e.message}")
        }
        val v = runCatching { if (body.isEmpty()) null else Json.parse(body) }.getOrNull()
        if (status !in 200..299) {
            throw FenecException(code(status), (v as? Row)?.string("error") ?: "HTTP $status")
        }
        return answer(v ?: throw FenecException(FenecException.Code.IO, "the server did not answer JSON ($status)"))
    }

    internal companion object {
        /** An HTTP status as the error kind the engine would have said. */
        fun code(status: Int) = when (status) {
            400 -> FenecException.Code.QUERY
            401, 403 -> FenecException.Code.DENIED
            404 -> FenecException.Code.NOT_FOUND
            409 -> FenecException.Code.DUPLICATE
            else -> FenecException.Code.IO
        }

        /** The endpoint's answer as the library's: rows come as an array. */
        fun answer(v: Any): Answer = when {
            v is List<*> -> {
                val rows = v.map { it as Row }
                Answer.Rows(rows.firstOrNull()?.keys?.toList() ?: emptyList(), rows)
            }
            v is Row && v.long("affected") != null -> Answer.Affected(v.long("affected")!!)
            v is Row && v.list("collections") != null -> Answer.Schemas(v.list("collections")!!.map { it as Row })
            v is Row -> Answer.Ok(v.string("message") ?: Json.write(v))
            else -> Answer.Ok(Json.write(v))
        }
    }
}

/** A fenec-server over HTTP, with no local file: every query goes to the server, through the same builder. */
fun Fenec.Companion.connect(url: String, token: String? = null): FenecRemote = FenecRemote(url, token)
