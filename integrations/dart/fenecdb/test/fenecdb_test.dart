import 'dart:io';
import 'dart:math';
import 'dart:typed_data';

import 'package:fenecdb/fenecdb.dart';
import 'package:fenecdb/src/values.dart' show iso, number, writeJson;
import 'package:test/test.dart';

/// A file of its own in a directory of its own, as an app's documents
/// directory holds one.
String scratch() => '${Directory.systemTemp.createTempSync('fenecdb-dart').path}/app.fenec';

Future<FenecCode?> codeOf(Future<Object?> f) async {
  try {
    await f;
    return null;
  } on FenecException catch (e) {
    return e.code;
  }
}

void main() {
  test('a statement and its rows', () async {
    final db = await Fenec.memory();
    final created = await db.run('create collection notes (title text, stars int, embed vector<3> @hnsw(cosine))');
    expect(created.message, 'collection `notes` created');
    final n = await db.execute('put notes [{title: \$1, stars: 5, embed: \$2}, {title: \$3, stars: 3, embed: \$4}]', [
      'oasis',
      Float32List.fromList([1, 0, 0]),
      'dunes',
      [0, 1, 0],
    ]);
    expect(n, 2);
    final rows = await db.query('get notes select title, stars order stars desc');
    expect(rows.map((r) => r['title']), ['oasis', 'dunes']);
    expect(rows[0]['stars'], 5);
    expect(rows[0].keys, ['title', 'stars']);
    await db.close();
  });

  test('near scores are exact: the vector goes over as its bytes', () async {
    final db = await Fenec.memory();
    await db.execute('create collection v (n int, e vector<2> @hnsw(l2))');
    await db.execute('put v [{n: 1, e: [1, 0]}, {n: 2, e: [0, 1]}, {n: 3, e: [0.5, 0.25]}]');
    final rows = await db.query('get v select n near e \$1 limit 3', [
      Float32List.fromList([1, 0])
    ]);
    expect(rows.map((r) => r['n']), [1, 3, 2]);
    double f32(double x) => (Float32List(1)..[0] = x)[0];
    final third = f32(sqrt(f32(f32(0.25) + f32(0.0625))));
    expect(rows.map((r) => f32((r['_score'] as num).toDouble())), [0, third, f32(sqrt(2))]);
    // A list of numbers is a vector too, as a page's are.
    expect(
        (await db.query('get v select n near e \$1 limit 1', [
          [0.0, 1.0]
        ]))
            .first['n'],
        2);
    expect((await db.query('get v select e where n = 3')).first['e'], [0.5, 0.25]);
    await db.close();
  });

  test('the builder writes and reads', () async {
    final db = await Fenec.memory();
    await db.execute('create collection notes (title text, stars int, at timestamp)');
    final notes = db.from('notes');
    final at = DateTime.fromMillisecondsSinceEpoch(1789821296789, isUtc: true);
    expect(
        await notes.insert([
          {'title': 'a', 'stars': 4, 'at': at},
          {'title': 'b', 'stars': 2},
        ]),
        2);
    expect(notes.toInsert({'title': 'a', 'stars': 4}).text, 'put notes {title: \$1, stars: \$2}');
    expect(notes.toInsert({'at': at.toLocal()}).params, ['2026-09-19T12:34:56.789Z']);
    expect((await notes.order('stars', 'desc').rows()).map((r) => r['title']), ['a', 'b']);
    expect((await notes.where('title', 'a').first())?['stars'], 4);
    expect(await notes.where('title', 'zz').first(), isNull);
    expect(await notes.count(), 2);
    expect(await notes.where('stars', '>', 3).update({'stars': 5}), 1);
    expect(await notes.where('stars', 5).delete(), 1);
    await db.close();
  });

  test('marks answer in the row, facets beside the rows', () async {
    final db = await Fenec.memory();
    await db.execute('create collection docs (body text @text, kind text)');
    await db.execute('put docs [{body: "rust is fast", kind: "a"}, {body: "rust and go", kind: "a"}, '
        '{body: "python", kind: "b"}]');
    final docs = db.from('docs');
    final hits = await docs.highlight('body').match('body', 'rust').rows();
    expect([
      for (final r in hits) r['highlight(body)']
    ], [
      [
        [0, 4]
      ],
      [
        [0, 4]
      ]
    ]);
    final tagged = await docs.select(['kind']).highlight('body', pre: '[', post: ']').match('body', 'fast').rows();
    expect(tagged.single['highlight(body)'], 'rust is [fast]');
    final snip = await docs.snippet('body', 2).match('body', 'fast').first();
    expect((snip!['snippet(body)'] as Map)['marks'], isNotEmpty);
    expect(hits.facets, isNull);
    final a = await docs.facet('kind').limit(1).answer();
    expect(a.rows, hasLength(1));
    expect(a.facets, {
      'kind': [const FacetCount('a', 2), const FacetCount('b', 1)]
    });
    expect((await docs.where('kind', 'b').facet('kind').rows()).facets, {
      'kind': [const FacetCount('b', 1)]
    });
    await db.close();
  });

  test('a json field takes its list as written', () async {
    final db = await Fenec.memory();
    await db.execute('create collection t (meta json)');
    await db.execute('put t {meta: \$1}', [
      [0.1, 0.2]
    ]);
    expect((await db.query('get t select meta')).first['meta'], [0.1, 0.2]);
    await db.close();
  });

  test('errors carry their kind', () async {
    final db = await Fenec.memory();
    await db.execute('create collection t (a int @unique)');
    await db.execute('put t {a: 1}');
    for (final (text, code) in [
      ('get nowhere', FenecCode.notFound),
      ('broken query', FenecCode.query),
      ('create collection t (a int)', FenecCode.exists),
      ('put t {a: 1}', FenecCode.duplicate),
      ('put t {a: "x"}', FenecCode.type),
    ]) {
      expect(await codeOf(db.run(text)), code, reason: text);
    }
    expect(
        () => db.from('t; drop collection t'),
        throwsA(isA<FenecException>()
            .having((e) => e.code, 'code', FenecCode.builder)
            .having((e) => e.message, 'message', 'invalid collection name: "t; drop collection t"')));
    await db.close();
    expect(await codeOf(db.run('get t')), FenecCode.misuse);
    await db.close();
  });

  test('a require not met is refused, and writes nothing', () async {
    final db = await Fenec.memory();
    await db.execute('create collection accounts (balance int)');
    await db.execute('put accounts {id: 1, balance: 5}');
    final accounts = db.from('accounts');
    expect(await codeOf(accounts.where('id', 2).update({'balance': 0}, require: 1)), FenecCode.unmet);
    expect((await accounts.rows()).map((r) => r['balance']), [5]);
    expect(await accounts.where('id', 1).update({'balance': 3}, require: 1), 1);
    await db.close();
  });

  test('a file is there again, and open once', () async {
    final path = scratch();
    var db = await Fenec.open(path);
    await db.execute('create collection t (title text, e vector<2> @hnsw(cosine))');
    for (var i = 0; i < 20; i++) {
      await db.execute('put t {title: \$1, e: \$2}', [
        'n$i',
        Float32List.fromList([i / 10, 1])
      ]);
    }
    expect(await codeOf(Fenec.open(path)), FenecCode.locked);
    await db.checkpoint();
    await db.close();
    db = await Fenec.open(path, flags: Fenec.noSync);
    expect(await db.from('t').count(), 20);
    await db.execute('put t {title: "after"}');
    await db.flush();
    await db.sync();
    await db.close();
    db = await Fenec.open(path, flags: Fenec.inMemory);
    expect(await db.from('t').count(), 21);
    final near = await db.from('t').select(['title']).near('e', Float32List.fromList([0, 1])).limit(1).rows();
    expect(near.first['title'], 'n0');
    await db.close();
  });

  test('many calls at once share one database', () async {
    final db = await Fenec.open(scratch());
    await db.execute('create collection t (w int, e vector<2> @hnsw(l2))');
    await Future.wait([
      for (var w = 0; w < 8; w++)
        () async {
          for (var i = 0; i < 20; i++) {
            if (w.isEven) {
              await db.execute('put t {w: \$1, e: \$2}', [
                w,
                [w, i]
              ]);
            } else {
              await db.query('get t near e \$1 limit 3', [
                [0, 1]
              ]);
            }
          }
        }()
    ]);
    expect(await db.from('t').count(), 80);
    await db.close();
  });

  test('values write as JSON the engine reads', () {
    expect(writeJson({'s': 'a"b\n\u0001ç😀', 'n': -1, 'f': 0.5, 'w': 2024.0, 'ok': true, 'none': null}),
        '{"s":"a\\"b\\n\\u0001ç😀","n":-1,"f":0.5,"w":2024,"ok":true,"none":null}');
    expect(number(double.nan), 'null');
    expect(iso(DateTime.fromMillisecondsSinceEpoch(-1, isUtc: true)), '1969-12-31T23:59:59.999Z');
  });

  test('the version is the library\'s', () async {
    expect((await Fenec.version()).split('.'), hasLength(3));
  });
}
