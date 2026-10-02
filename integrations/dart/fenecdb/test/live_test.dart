import 'dart:async';

import 'package:fenecdb/fenecdb.dart';
import 'package:test/test.dart';

Future<Fenec> todos() async {
  final db = await Fenec.memory();
  await db.execute('create collection todos (title text, done bool @hash); create collection other (n int)');
  await db.from('todos').insert([
    {'title': 'milk', 'done': false},
    {'title': 'bread', 'done': true},
  ]);
  return db;
}

/// Waits until [done] holds, or fails after a few seconds.
Future<void> until(String what, bool Function() done) async {
  for (var i = 0; i < 500 && !done(); i++) {
    await Future<void>.delayed(const Duration(milliseconds: 10));
  }
  expect(done(), isTrue, reason: what);
}

Future<void> settle() => Future<void>.delayed(const Duration(milliseconds: 100));

void main() {
  test('a stream has its rows now and after each write', () async {
    final db = await todos();
    final seen = <List<Object?>>[];
    final sub = db
        .live(db.from('todos').select(['title']).where('done', false))
        .listen((rows) => seen.add([for (final r in rows) r['title']]));
    await until('the first rows', () => seen.length == 1);
    expect(seen[0], ['milk']);
    await db.from('todos').insert({'title': 'eggs', 'done': false});
    await until('the rows after a put', () => seen.length == 2);
    expect(seen[1], ['milk', 'eggs']);
    // A write to a collection it does not read runs nothing.
    await db.execute('put other {n: 1}');
    await settle();
    expect(seen, hasLength(2));
    await db.from('todos').where('title', 'milk').update({'done': true});
    await until('the rows after a set', () => seen.length == 3);
    expect(seen[2], ['eggs']);
    await sub.cancel();
    await db.close();
  });

  test('the writes of a burst run it once', () async {
    final db = await todos();
    final all = db.from('todos');
    final runs = <int>[];
    final sub = db.live(all).listen((rows) => runs.add(rows.length));
    await until('the first rows', () => runs.length == 1);
    await Future.wait([
      for (var i = 0; i < 10; i++) all.insert({'title': 't$i', 'done': false})
    ]);
    await until('the rows after the burst', () => runs.last == 12);
    await settle();
    expect(runs, [2, 12]);
    await db.execute('put todos {title: "x"}; put todos {title: "y"}; put other {n: 2}');
    await until('the rows after a text', () => runs.last == 14);
    await settle();
    expect(runs, hasLength(3));
    // A text that fails is put back whole and runs nothing.
    await expectLater(db.execute('put todos {title: "z"}; put other {n: "no"}'), throwsA(isA<FenecException>()));
    await settle();
    expect(runs, hasLength(3));
    await sub.cancel();
    await all.insert({'title': 'after', 'done': false});
    await settle();
    expect(runs, hasLength(3));
    await db.close();
  });

  test('a text names what it reads, and a drop runs everything', () async {
    final db = await todos();
    final counts = <Object?>[];
    final errors = <Object>[];
    final a = db.liveText('get todos count', collections: ['todos']).listen((r) => counts.add(r.first['count']));
    final b = db.liveText('get other count').listen((_) {}, onError: errors.add);
    await until('the first rows', () => counts.length == 1);
    await db.execute('put todos {title: "t"}');
    await until('the count', () => counts.last == 3);
    await db.execute('drop collection other');
    await until('the error', () => errors.isNotEmpty);
    expect((errors.first as FenecException).code, FenecCode.notFound);
    await expectLater(db.live(db.from('nowhere')).first,
        throwsA(isA<FenecException>().having((e) => e.code, 'code', FenecCode.notFound)));
    await a.cancel();
    await b.cancel();
    await db.close();
  });
}
