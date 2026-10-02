// ignore_for_file: avoid_print
// The Notes smoke steps (examples/README.md), on a fresh file in [dir]:
// a line each, an exception at the first that fails. Plain Dart, so it
// runs under `flutter test` and without Flutter alike.
import 'dart:async';
import 'dart:io';

import 'package:fenecdb_notes/notes.dart';

void check(bool ok, String step, [Object? got]) {
  if (!ok) throw StateError('$step: got $got');
  print('ok  $step');
}

List<Object?> titles(List<Map<String, Object?>> rows) => [for (final r in rows) r['title']];

Future<void> smoke(Directory dir, String schema) async {
  final path = '${dir.path}/notes.fenec';
  var notes = await Notes.open(path, schema);
  final db = notes.db;

  final count = await db.from('notes').count();
  check(count == 4, 'schema applied, 4 seeds', count);

  final hello = embed('hello');
  final nonZero = [
    for (var i = 0; i < hello.length; i++)
      if (hello[i] != 0) i,
  ];
  check(nonZero.join(',') == '24,36,46,48,62', 'toy embedding of "hello"', nonZero);

  final list = await notes.list().rows();
  check(list.first['title'] == 'Book club', 'newest first', titles(list));

  final work = titles(await notes.list(tag: 'work').rows());
  check(work.join('|') == 'Book flights|Release checklist', 'tag work', work);

  final open = await notes.list(openOnly: true).rows();
  check(open.length == 3, 'open notes', titles(open));

  final matched = await notes.match('release docs').rows();
  check(matched.first['title'] == 'Release checklist', 'match "release docs"', titles(matched));

  final near = await notes.similar('flights to Istanbul').rows();
  check(near.first['title'] == 'Book flights', 'near "flights to Istanbul"', titles(near));

  final fused = await notes.search('desert fox').rows();
  check(fused.first['title'] == 'Book club', 'fuse "desert fox"', titles(fused));

  // A live query of the open notes hands over its rows again after a write.
  final four = Completer<void>();
  final sub = notes.live(notes.list(openOnly: true)).listen((rows) {
    if (rows.length == 4 && !four.isCompleted) four.complete();
  });
  await notes.add('Call mom', 'Ask about the weekend.', ['home']);
  await four.future.timeout(const Duration(seconds: 5));
  await sub.cancel();
  check(true, 'live query saw "Call mom"');

  final groceries = await db.from('notes').where('title', 'Groceries').first();
  await notes.finish(groceries!['id'] as int);
  final stillOpen = await notes.list(openOnly: true).rows();
  check(stillOpen.length == 3, 'Groceries done', titles(stillOpen));

  await db.close();
  notes = await Notes.open(path, schema);
  final again = await notes.db.from('notes').count();
  check(again == 5, 'reopened: 5 notes kept', again);
  await notes.db.close();
}

// Without Flutter: dart run test/smoke.dart, FENEC_LIBRARY naming the library.
Future<void> main() async {
  final dir = await Directory.systemTemp.createTemp('notes');
  try {
    await smoke(dir, File('schema.fenecql').readAsStringSync());
  } finally {
    await dir.delete(recursive: true);
  }
}
