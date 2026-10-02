/// fenecdb in a Flutter app: the `fenecdb` package, its native library
/// bundled by this plugin for iOS and macOS (linked into the app, found
/// in the process) and Android (`libfenec_ffi.so` for each ABI).
///
/// ```dart
/// final dir = await getApplicationDocumentsDirectory();   // path_provider
/// final db = await Fenec.open('${dir.path}/app.fenec');
///
/// StreamBuilder(
///   stream: db.live(db.from('todos').where('done', false)),
///   builder: (context, snap) => ListView(children: [
///     for (final t in snap.data ?? const []) Text(t['title'] as String),
///   ]),
/// );
/// ```
library;

export 'package:fenecdb/fenecdb.dart';
