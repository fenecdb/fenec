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

/* The codes a call returns. 1 to 10 and 14 are the engine's errors by kind. */
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
#define FENEC_UNMET 14      /* a write's `require <n>` not met; its block put back */

/* fenec_open's flags. */
#define FENEC_OPEN_NO_SYNC 1   /* writes wait for fenec_sync, not an fsync each */
#define FENEC_OPEN_IN_MEMORY 2 /* read the file into memory rather than map it */
#define FENEC_OPEN_NO_AUTO_COMPACT 4 /* no compact on its own once half the file is dead */

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

/* fenec_schema's modes. */
#define FENEC_SCHEMA_PLAN 0     /* what an apply would do; writes nothing */
#define FENEC_SCHEMA_APPLY 1    /* migrations not yet recorded, then what only adds: one block */
#define FENEC_SCHEMA_FOLLOW 2   /* compares a database another owns */
#define FENEC_SCHEMA_DESCRIBE 3 /* the database's schema as a description; text unread */

/*
 * The database and a schema declared in code: text is the JSON description
 * every SDK's declarations compile to, {"format":1,"collections":[...],
 * "migrations":[...]}. Writes {"kind":"schema","applied":..,"ran":..,
 * "migrations":[n...],"statements":[...],"refusals":[{"kind","collection",
 * "field","message","fix"}...]}; nothing is applied while anything is
 * refused.
 */
int32_t fenec_schema(uint64_t handle, const uint8_t *text, size_t text_len, uint32_t mode,
                     char **out, size_t *out_len);

/* Every write so far written and fsynced. */
int32_t fenec_sync(uint64_t handle, char **out, size_t *out_len);

/* Every write so far handed to the system, no fsync: outlives the app, not power. */
int32_t fenec_flush(uint64_t handle, char **out, size_t *out_len);

/* The file written anew as an image, graphs and all. */
int32_t fenec_checkpoint(uint64_t handle, char **out, size_t *out_len);

/*
 * Builds the hash, text, ordered and sparse indexes an open leaves for their
 * first read: those `only` names (collections and collection.fields by
 * commas), or every one when it is empty. Each under the read lock on its
 * own; call it off the main thread after the open. Writes {"built":N}.
 */
int32_t fenec_warm(uint64_t handle, const uint8_t *only, size_t only_len, char **out,
                   size_t *out_len);

/*
 * Sync with a server: a state machine with no I/O of its own. The binding
 * makes the requests and the event stream with its platform's HTTP client
 * (TLS, the system's trust store), feeds what happened, and performs the
 * actions each call writes, a JSON array of
 *   {"do":"request","id":N,"method":..,"url":..,"headers":{..},"body":..|null}
 *   {"do":"stream","id":N,"url":..,"headers":{..}}   an SSE body, fed as it comes
 *   {"do":"cancel","id":N}     {"do":"wait","id":N,"ms":M}     {"do":"token"}
 *   {"do":"changed"}           {"do":"status"}
 *   {"do":"refused","status":S,"message":..,"query":..}
 * Once started, a write through fenec_query to a synced collection is
 * applied at once and queued for the server in the same block.
 */
#define FENEC_SYNC_POLL 0      /* nothing happened: what is due */
#define FENEC_SYNC_RESPONSE 1  /* a request's answer: status (0: none), seq, body */
#define FENEC_SYNC_OPENED 2    /* a stream's status; its body when not 200 */
#define FENEC_SYNC_BYTES 3     /* a piece of a stream's body */
#define FENEC_SYNC_CLOSED 4    /* a stream ended (or never opened): why */
#define FENEC_SYNC_TIMER 5     /* a wait ran out */
#define FENEC_SYNC_SIGNAL 6    /* {"online":bool} | {"token":".."} | {"stop":true} */

/* config: {"url":..,"token":..,"seed":"<32 hex>","shapes":[{collection,where?,select?,key?}]}. */
int32_t fenec_sync_start(uint64_t handle, const uint8_t *config, size_t config_len,
                         char **out, size_t *out_len);

/* Tells the sync what happened (FENEC_SYNC_*); writes the actions now due. */
int32_t fenec_sync_feed(uint64_t handle, uint32_t kind, uint64_t id, int32_t status,
                        uint64_t seq, const uint8_t *bytes, size_t len,
                        char **out, size_t *out_len);

/* {"state":"online"|"offline"|"catching_up","pending":N,"error":..,"shapes":[..]}. */
int32_t fenec_sync_status(uint64_t handle, char **out, size_t *out_len);

/* Frees text a call wrote through `out`. */
void fenec_free_string(char *s);

#ifdef __cplusplus
}
#endif

#endif /* FENEC_H */
