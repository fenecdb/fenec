# fenecdb for Dart

fenecdb embedded in a Dart or Flutter app: the database is a file on the
device, the engine the native library (`crates/fenec-ffi`) the server and the
browser module share their code and file format with -- on its own, or a
replica a server keeps in step.
In a Flutter app, depend on `fenecdb_flutter`, which bundles the library for
iOS, Android and macOS; elsewhere, point `Fenec.library` (or
`FENEC_LIBRARY`) at `libfenec_ffi`.

```dart
import 'package:fenecdb/fenecdb.dart';

final db = await Fenec.open('${dir.path}/app.fenec');
await db.execute('create collection todos (title text, done bool @hash)');
await db.from('todos').insert({'title': 'milk', 'done': false});
final open = await db.from('todos').where('done', false).rows();
final near = await db.query(r'get notes near embed $1 limit 5', [embedding]); // a Float32List

db.live(db.from('todos').where('done', false)).listen((rows) => print(rows));
```

Every call runs on an isolate the database keeps for itself, since a call
may wait for the lock or an fsync and an FFI call blocks its isolate. A live
query is a Stream, run again after a write to a collection it reads, once a
burst. Every write is fsynced before it returns unless the file is opened
with `flags: Fenec.noSync`, which leaves the writes for `sync()`; `flush()`
hands them to the system, which outlives the app being killed. An open
leaves the hash, text, ordered and sparse indexes for their first read;
`warm(['docs'])` builds them on the database's isolate as the app starts,
so the first search does not (a `@text` index of 100 000 products: 141
ms), and answers how many it built. The builder's `facet(field, ranges:
[0, 25, 50])` counts by ranges of numbers, each value `[from, to]`, and
`disjunctive: true` counts past the filter's own condition on the field.

## Sync with a server

```dart
final db = await Fenec.openSynced(
  url: 'https://api.example.com', token: jwt,
  shapes: [Shape('todos', where: {'owner': me}, key: 'key')],
  path: '${dir.path}/todos.fenec',
  tokenProvider: refreshToken,
);
await db.replica!.ready();
```

The same `Fenec`: reads and live queries are the file's, a write to a
shape's collection shows at once and is sent -- queued in the file while
the server cannot be reached, under an idempotency key so it lands once --
and one the server refuses is put back and comes on `replica.refusals`.
`replica.status` says `online`, `offline` or `catchingUp`, the writes not
yet answered and the last error, and `statuses` streams it; call
`replica.resume()` on `AppLifecycleState.resumed`. `openSynced`, not
`sync`: Dart has no static and instance member of one name, and `sync()` is
the fsync. The requests and the stream are `dart:io`'s `HttpClient`'s, so
a server is reached through TLS. `Fenec.connect(url, token:)` is a server
with no file: every query a request, the same builder. Its `batch([...],
idempotencyKey: id)` runs the builder's `toInsert`/`toUpdate`/`toDelete` as
one block, all or none, answering a `BatchAnswer` (`results`, `seq`,
`replayed`); a `FenecException` that stops it carries `status` (412 and
`unmet` for a `require` not met) and `at`, the statement from 0.
`withIdempotencyKey(key)` is a copy whose every write -- `run`, the
builder's -- carries the key, so it lands once however often it is sent.

The query builder makes the text the JavaScript builder makes of the same
chain, to the byte: `integrations/builder-golden.json` holds the chains,
and `make dart-test` runs every one. To see the text and parameters a chain
builds, for logging or a test, `toFenecQL()` returns them and runs nothing.
