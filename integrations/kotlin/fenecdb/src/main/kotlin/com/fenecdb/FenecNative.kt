package com.fenecdb

import java.io.File

/**
 * The JNI functions of the native library (crates/fenec-ffi with its `jni`
 * feature): the C ABI's calls, each answering one `byte[]` -- its first
 * byte the call's code, the rest the answer's JSON (an open: the handle's
 * digits). Text goes in as its UTF-8 bytes: JNI's own strings are
 * "modified UTF-8", which writes a character outside the BMP -- an emoji --
 * as two halves the engine's parser refuses.
 *
 * The library is `libfenec_ffi.so` from the AAR's `jniLibs` on Android;
 * on the JVM, the file the `fenec.library` system property or the
 * `FENEC_LIBRARY` variable names, else `fenec_ffi` on `java.library.path`.
 */
internal object FenecNative {
    init {
        val path = System.getProperty("fenec.library") ?: System.getenv("FENEC_LIBRARY")
        if (path != null) System.load(File(path).absolutePath) else System.loadLibrary("fenec_ffi")
    }

    @JvmStatic external fun version(): ByteArray

    @JvmStatic external fun open(path: ByteArray, flags: Int): ByteArray

    @JvmStatic external fun openMemory(): ByteArray

    @JvmStatic external fun close(handle: Long): ByteArray

    @JvmStatic external fun query(handle: Long, text: ByteArray, params: ByteArray, vectors: ByteArray?): ByteArray

    @JvmStatic external fun changes(handle: Long, since: Long): ByteArray

    @JvmStatic external fun sync(handle: Long): ByteArray

    @JvmStatic external fun flush(handle: Long): ByteArray

    @JvmStatic external fun checkpoint(handle: Long): ByteArray

    @JvmStatic external fun syncStart(handle: Long, config: ByteArray): ByteArray

    @JvmStatic external fun syncFeed(handle: Long, kind: Int, id: Long, status: Int, seq: Long, bytes: ByteArray?): ByteArray

    @JvmStatic external fun syncStatus(handle: Long): ByteArray

    /** The indexes an open left for their first read, built now (`fenec_warm`): `only` the names by commas, empty for all. */
    @JvmStatic external fun warm(handle: Long, only: ByteArray): ByteArray

    /** A schema declared as FenecQL against the database (`fenec_schema`): mode 0 plans, 1 applies. */
    @JvmStatic external fun schema(handle: Long, request: ByteArray, mode: Int): ByteArray

    /** A call's answer: its text, or the error it is thrown as. */
    fun answer(bytes: ByteArray): String {
        val code = bytes[0].toInt()
        val text = String(bytes, 1, bytes.size - 1, Charsets.UTF_8)
        if (code == 0) return text
        val v = runCatching { Json.parse(text) }.getOrNull() as? Map<*, *>
        val exact = (v?.get("exact") as? List<*>)?.map { (it as Number).toInt() }
        throw FenecException(FenecException.Code.of(code), v?.get("message") as? String ?: "error $code", exact)
    }
}
