# Notes -- Flutter

A notes app for iOS, Android and macOS with the database in a file on the
device: `fenecdb_flutter`, no server, no network. The plugin's own
[`example/`](../../integrations/dart/fenecdb_flutter/example) is the minimal
todo list; this is the fuller app every language's example is
([examples/README.md](../README.md)).

## What it shows

- the schema in [`schema.fenecql`](schema.fenecql), applied with
  `db.schema(...)` at every open: made the first time, checked after;
- notes created from a dialog, tapped to mark done;
- full-text search with `match` over `body @text`, fused with a vector
  search (`near ... fuse`) through a toy embedding;
- filters by tag (`tags has ...`) and open notes (`done @hash`), newest first
  (`order at desc` over `at @sorted`);
- the list is a live query: a `StreamBuilder` over `db.live(...)`, drawn
  again after every write, with nothing calling it;
- the file in the app's Application Support directory, kept across
  restarts.

## Prerequisites

Flutter 3.22 or newer, and Xcode or the Android SDK for the platform you run.

## Run

```sh
cd examples/flutter
flutter create --platforms=android,ios,macos --project-name fenecdb_notes .
flutter run
```

`flutter create .` makes the platform projects, which are not kept here, and
a `test/widget_test.dart` this app does not use: delete it. The app's data
is `notes.fenec` in `getApplicationSupportDirectory()`; uninstall the app to
start over.

## The core

```dart
final dir = await getApplicationSupportDirectory();
final db = await Fenec.open('${dir.path}/notes.fenec');
await db.schema(await rootBundle.loadString('schema.fenecql'));

await db.from('notes').insert({'title': title, 'body': body, 'tags': tags,
    'done': false, 'at': DateTime.now().toUtc(), 'embed': embed('$title $body')});

StreamBuilder(
  stream: db.live(db.from('notes').where('done', false).order('at', 'desc')),
  builder: (context, snap) => ListView(children: [/* a tile a note */]),
);
final hits = await db.from('notes').match('body', words).near('embed', embed(words)).fuse().rows();
```

All of it is in [`lib/notes.dart`](lib/notes.dart), which imports only
`package:fenecdb`; [`lib/main.dart`](lib/main.dart) is the screen.

## The toy embedding

`embed()` is a placeholder for a real model: hashed character trigrams
into 64 dimensions. It finds notes that share spellings with the query, not
meaning, and needs no download, so `near` and `fuse` run here as they would
with a real one. To use a real model, compute the vector in `embed()` --
TFLite or ONNX Runtime on the device, or an embeddings API -- and declare
`embed vector<N>` with the model's N in `schema.fenecql`.

## Against this repository's build

```sh
examples/flutter/run-tests.sh        # flutter test, or the data layer under plain Dart without Flutter
examples/flutter/run-tests.sh apk    # and the app built for Android
```

It writes a `pubspec_overrides.yaml` (ignored by git) pointing `fenecdb`
and `fenecdb_flutter` at `integrations/dart`, builds the native library for
this machine and runs [`test/notes_test.dart`](test/notes_test.dart): the
seeds, the searches, the filters, a live query and a reopen. CI runs it and
builds the Android app. A copy of this folder outside the repository takes
both packages from pub.dev.
