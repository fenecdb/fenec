# fenecdb for Dart

fenecdb embedded in a Dart or Flutter app: the database is a file on the
device, the engine the native library (`crates/fenec-ffi`) the server and the
browser module share their code and file format with. No server, no network.
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
hands them to the system, which outlives the app being killed.

The query builder makes the text the JavaScript builder makes of the same
chain, to the byte: `integrations/builder-golden.json` holds the chains,
and `make dart-test` runs every one.
