/*
 * fenecdb as a native library: the C ABI of crates/fenec-ffi.
 *
 * Written by hand, and held to the library by crates/fenec-ffi/tests/ffi.rs
 * (every function declared here is exported, and every export declared).
 *
 * A handle is a number, safe to use from several threads at once: a read
 * runs beside other reads, a write takes the database to itself. Every call
 * that takes one may block -- for the lock, an fsync, a whole open -- so
 * none belongs on an app's main thread.
 *
 * Every call returns 0 or an error code. Where it has an answer, or an
 * error to tell, it writes NUL-terminated UTF-8 JSON through `out` and its
 * length through `out_len` (either may be NULL); the caller frees it with
 * fenec_free_string. An error is {"kind":"error","message":"..."}.
 */
#ifndef FENEC_H
#define FENEC_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* The codes a call returns. 1 to 10 are the engine's errors by kind. */
#define FENEC_OK 0
#define FENEC_TYPE 1        /* a value that does not fit its field */
#define FENEC_NOT_FOUND 2   /* a collection, field or document not there */
#define FENEC_EXISTS 3      /* a collection there already */
#define FENEC_DUPLICATE 4   /* an insert over an id held, a @unique value held */
#define FENEC_CORRUPT 5     /* bytes that are not a fenecdb file */
#define FENEC_QUERY 6       /* a statement that does not parse or run */
#define FENEC_IO 7          /* the disk refused; writes stop until a reopen */
#define FENEC_PLUGIN 8
#define FENEC_READ_ONLY 9
#define FENEC_DENIED 10
#define FENEC_PANIC 11      /* a panic inside the library, caught */
#define FENEC_MISUSE 12     /* a handle not open, a NULL, text not UTF-8 */
#define FENEC_LOCKED 13     /* the file is open already, here or elsewhere */

/* fenec_open's flags. */
#define FENEC_OPEN_NO_SYNC 1   /* writes wait for fenec_sync, not an fsync each */
#define FENEC_OPEN_IN_MEMORY 2 /* read the file into memory rather than map it */

/* The library's version; static, not to be freed. */
const char *fenec_version(void);

/* Opens the file at path (made when missing) and writes its handle. */
int32_t fenec_open(const uint8_t *path, size_t path_len, uint32_t flags,
                   uint64_t *handle, char **out, size_t *out_len);

/* Opens a database held in memory alone. */
int32_t fenec_open_memory(uint64_t *handle, char **out, size_t *out_len);

/* Waits for the calls in flight, saves the graphs, syncs, lets the file go. */
int32_t fenec_close(uint64_t handle, char **out, size_t *out_len);

/*
 * Runs FenecQL. params: a JSON array for $1, $2 ... (length 0 for none).
 * vectors: the parameters that are vectors, as f32s -- for each, its place
 * and its length as little-endian uint32s, then its values -- where the
 * JSON holds null; NULL for none. Writes the answer:
 * {"kind":"rows","result":{"columns":[...],"rows":[...]}},
 * {"kind":"affected","count":N}, {"kind":"ok","message":...} or
 * {"kind":"schemas","collections":[...]}. An error holding "exact":[places]
 * asks for those parameters again inside the JSON.
 */
int32_t fenec_query(uint64_t handle, const uint8_t *text, size_t text_len,
                    const uint8_t *params, size_t params_len,
                    const uint8_t *vectors, size_t vectors_len,
                    char **out, size_t *out_len);

/* {"seq":N,"horizon":M,"collections":[...]|null}: what changed since `since`. */
int32_t fenec_changes(uint64_t handle, uint64_t since, char **out, size_t *out_len);

/* Every write so far written and fsynced. */
int32_t fenec_sync(uint64_t handle, char **out, size_t *out_len);

/* Every write so far handed to the system, no fsync: outlives the app, not power. */
int32_t fenec_flush(uint64_t handle, char **out, size_t *out_len);

/* The file written anew as an image, graphs and all. */
int32_t fenec_checkpoint(uint64_t handle, char **out, size_t *out_len);

/* Frees text a call wrote through `out`. */
void fenec_free_string(char *s);

#ifdef __cplusplus
}
#endif

#endif /* FENEC_H */
