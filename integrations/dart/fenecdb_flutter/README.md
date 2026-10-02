# fenecdb for Flutter

The `fenecdb` package in a Flutter app, with its native library bundled:
the XCFramework on iOS and macOS, `libfenec_ffi.so` for arm64-v8a,
armeabi-v7a and x86_64 on Android.

```dart
import 'package:fenecdb_flutter/fenecdb_flutter.dart';
import 'package:path_provider/path_provider.dart';

final dir = await getApplicationDocumentsDirectory();
final db = await Fenec.open('${dir.path}/app.fenec');

StreamBuilder(
  stream: db.live(db.from('todos').where('done', false)),
  builder: (context, snap) => ListView(children: [
    for (final t in snap.data ?? const []) Text(t['title'] as String),
  ]),
);
```

`example/` is a todo list over a live query. In the repository,
`prepare.sh` copies the libraries in from the Swift and Kotlin builds
(`integrations/swift/build-xcframework.sh`, `integrations/kotlin/build-aar.sh`).
