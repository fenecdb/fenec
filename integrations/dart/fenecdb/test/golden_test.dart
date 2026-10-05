import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:fenecdb/fenecdb.dart';
import 'package:test/test.dart';

/// The query builder held to integrations/builder-golden.json: the text and
/// parameters the JavaScript builder makes of every chain there, refusals by
/// their message, the endpoints' statements read off a recording executor.
List<Map<String, Object?>> golden() {
  final path = Platform.environment['FENEC_GOLDEN'] ??
      [
        for (var d = Directory.current; d.parent.path != d.path; d = d.parent)
          '${d.path}/integrations/builder-golden.json'
      ].firstWhere((p) => File(p).existsSync());
  return (jsonDecode(File(path).readAsStringSync()) as List).cast<Map<String, Object?>>();
}

/// An argument as a Dart caller hands it over: `$date` a DateTime, `$f32` a
/// Float32List, an object a map.
Object? value(Object? v) {
  if (v is Map) {
    if (v.containsKey(r'$date')) return DateTime.parse(v[r'$date'] as String);
    if (v.containsKey(r'$f32')) {
      return Float32List.fromList((v[r'$f32'] as List).map((x) => (x as num).toDouble()).toList());
    }
    if (v.containsKey(r'$inc')) return Computed.inc(v[r'$inc']);
    if (v.containsKey(r'$expr')) {
      final e = v[r'$expr'] as List;
      return Computed.expr(e[0] as String, [for (final x in e.skip(1)) value(x)]);
    }
    if (v.containsKey(r'$bucket')) {
      final b = v[r'$bucket'] as List;
      return Computed.bucket(b[0] as String, b[1] as String);
    }
    if (v.containsKey(r'$countDistinct')) return Computed.countDistinct(v[r'$countDistinct'] as String);
    for (final (key, pick) in [(r'$first', Computed.first), (r'$last', Computed.last)]) {
      if (v.containsKey(key)) {
        final f = v[key] as List;
        return pick(f[0] as String, f.length > 1 ? f[1] as String : null);
      }
    }
    if (v.containsKey(r'$as')) {
      final a = v[r'$as'] as List;
      return (value(a[0]) as Computed).as(a[1] as String);
    }
    return {for (final e in v.entries) e.key as String: value(e.value)};
  }
  if (v is List) return [for (final x in v) value(x)];
  return v;
}

Object cond(Object? v) {
  final m = v as Map;
  if (m.containsKey(r'$or')) return Cond.or([for (final c in m[r'$or'] as List) cond(c)]);
  if (m.containsKey(r'$and')) return Cond.and([for (final c in m[r'$and'] as List) cond(c)]);
  if (m.containsKey(r'$not')) return Cond.not(cond(m[r'$not']));
  if (m.containsKey(r'$raw')) {
    final r = m[r'$raw'] as List;
    return Cond.raw(r[0] as String, [for (final x in r.skip(1)) value(x)]);
  }
  return value(m)!;
}

Object? opt(List a, int at, String name) => a.length > at && a[at] is Map ? (a[at] as Map)[name] : null;

List<SortKey> keys(Object? v) {
  if (v == null) return const [];
  if (v is String) return [SortKey(v)];
  return [
    for (final k in v as List)
      k is String
          ? SortKey(k)
          : SortKey((k as List)[0] as String, k.length > 1 ? k[1] as String : 'asc',
              k.length > 2 ? (k[2] as Map)['collate'] as String? : null)
  ];
}

Query step(Query q, String op, List a) => switch ((op, a.length)) {
      ('select', _) => q.select([for (final c in a) value(c)!]),
      ('where', 3) => q.where(a[0] as String, a[1], value(a[2])),
      ('where', 2) => q.where(a[0] as String, value(a[1])),
      ('where', _) => q.where(cond(a[0])),
      ('orWhere', 3) => q.orWhere(a[0] as String, a[1], value(a[2])),
      ('orWhere', 2) => q.orWhere(a[0] as String, value(a[1])),
      ('orWhere', _) => q.orWhere(cond(a[0])),
      ('near', _) =>
        q.near(a[0] as String, value(a[1]), ef: opt(a, 2, 'ef') as int?, exact: opt(a, 2, 'exact') as bool? ?? false),
      ('rerank', _) => q.rerank(a[0] as String, value(a[1]), candidates: opt(a, 2, 'candidates') as int?),
      ('match', _) => q.match(a[0] as String, a[1] as String),
      ('fuse', _) => q.fuse(k: opt(a, 0, 'k') as int?, candidates: opt(a, 0, 'candidates') as int?),
      ('highlight', _) => q.highlight(a[0] as String, pre: opt(a, 1, 'pre'), post: opt(a, 1, 'post')),
      ('snippet', _) => q.snippet(a[0] as String, a[1] as int,
          ellipsis: opt(a, 2, 'ellipsis'), pre: opt(a, 2, 'pre'), post: opt(a, 2, 'post')),
      ('facet', _) => q.facet(a[0] as String,
          top: opt(a, 1, 'top') as int?,
          ranges: opt(a, 1, 'ranges') as List<Object?>?,
          disjunctive: opt(a, 1, 'disjunctive')),
      // One key as itself, several (or none) as the list they are.
      ('group', 1) => q.group(value(a[0])!),
      ('group', _) => q.group([for (final k in a) value(k)!]),
      ('order', _) => q.order(a[0] as String, a.length > 1 ? a[1] as String : 'asc', opt(a, 2, 'collate') as String?),
      ('limit', _) => q.limit(a[0] as int),
      ('offset', _) => q.offset(a[0] as int),
      ('require', _) => q.require(a[0] as int),
      ('lookup', _) => q.lookup(a[0] as String,
          on: opt(a, 1, 'on') as String?,
          parentKey: opt(a, 1, 'parentKey') as String?,
          select: switch (opt(a, 1, 'select')) {
            null => null,
            String s => [s],
            final l => (l as List).cast<String>(),
          },
          where: opt(a, 1, 'where') == null ? null : cond(opt(a, 1, 'where')),
          required: opt(a, 1, 'required') as bool? ?? false,
          order: keys(opt(a, 1, 'order')),
          limit: opt(a, 1, 'limit') as int?,
          offset: opt(a, 1, 'offset') as int?),
      _ => throw StateError('no builder step $op'),
    };

/// The statement a chain makes, or the builder's refusal.
Future<(String?, List<Object?>?, String?)> run(List steps) async {
  ({String text, List<Object?> params})? sent;
  try {
    var q = Query.from(((steps[0] as Map)['args'] as List)[0] as String).bind((text, params) async {
      sent = (text: text, params: params);
      // The binding's answers are made by its own reader; these stand in.
      if (RegExp(r'^(put|set|del) ').hasMatch(text)) return Answer.of({'kind': 'affected', 'count': 0});
      return Answer.of({
        'kind': 'rows',
        'result': {
          'columns': [],
          'rows': text.endsWith(' count')
              ? [
                  {'count': 0}
                ]
              : []
        }
      });
    });
    for (final s in steps.skip(1).cast<Map>()) {
      final op = s['op'] as String;
      final a = (s['args'] as List?) ?? const [];
      bool all(int at) => opt(a, at, 'all') as bool? ?? false;
      bool absent() => opt(a, 1, 'ifAbsent') as bool? ?? false;
      int? require(int at) => opt(a, at, 'require') as int?;
      final made = switch (op) {
        'toFenecQL' => q.toFenecQL(),
        'toInsert' => q.toInsert(value(a[0])!, ifAbsent: absent(), require: require(1)),
        'toUpdate' => q.toUpdate(value(a[0])!, all: all(1), require: require(1)),
        'toDelete' => q.toDelete(all: all(0), require: require(0)),
        _ => null,
      };
      if (made != null) return (made.text, made.params, null);
      final Future<Object?>? ran = switch (op) {
        'rows' => q.rows(),
        'first' => q.first(),
        'count' => q.count(),
        'explain' => q.explain(),
        'insert' => q.insert(value(a[0])!, ifAbsent: absent(), require: require(1)),
        'update' => q.update(value(a[0])!, all: all(1), require: require(1)),
        'delete' => q.delete(all: all(0), require: require(0)),
        _ => null,
      };
      if (ran != null) {
        await ran;
        return (sent?.text, sent?.params, null);
      }
      q = step(q, op, a);
    }
    return (null, null, 'a chain ends with a statement');
  } on FenecException catch (e) {
    return (null, null, e.message);
  }
}

/// JSON compared by value: a number the file writes as 1 and the builder
/// holds as 1.0 is one number.
bool same(Object? a, Object? b) {
  if (a is num && b is num) return a.toDouble() == b.toDouble();
  if (a is List && b is List) {
    return a.length == b.length && [for (var i = 0; i < a.length; i++) i].every((i) => same(a[i], b[i]));
  }
  if (a is Map && b is Map) return a.length == b.length && a.keys.every((k) => b.containsKey(k) && same(a[k], b[k]));
  return a == b;
}

void main() {
  final cases = golden();

  test('the file holds enough cases', () => expect(cases.length, greaterThanOrEqualTo(150)));

  for (final c in cases) {
    test('golden: ${c['name']}', () async {
      final (text, params, error) = await run(c['steps'] as List);
      if (c['error'] != null) {
        expect(error, c['error'], reason: 'made $text');
        return;
      }
      expect(error, isNull);
      expect(text, c['text']);
      final got = jsonDecode(jsonEncode(params, toEncodable: (x) => x is Float32List ? x.toList() : x));
      expect(same(got, c['params']), isTrue, reason: 'params $got, want ${c['params']}');
    });
  }

  test('the builder answers as the text', () async {
    final db = await Fenec.memory();
    await db.execute('create collection shelf (title text, year int @sorted, lang text @hash, tags [text], '
        'body text @text, embed vector<3> @hnsw(cosine)); create collection notes (doc_id int @hash, stars int)');
    final shelf = db.from('shelf');
    expect(
        await shelf.insert([
          {
            'title': 'Night at the oasis',
            'year': 2024,
            'lang': 'en',
            'tags': ['desert'],
            'body': 'a night under the stars at the oasis',
            'embed': Float32List.fromList([0.1, 0.2, 0.3])
          },
          {
            'title': 'Dunes',
            'year': 2021,
            'lang': 'en',
            'tags': ['desert', 'sand'],
            'body': 'dunes move with the wind',
            'embed': Float32List.fromList([0.9, 0.1, 0])
          },
          {
            'title': 'Kum',
            'year': 2023,
            'lang': 'tr',
            'tags': ['sand'],
            'body': 'kum ve rüzgar',
            'embed': Float32List.fromList([0.2, 0.8, 0.1])
          },
        ]),
        3);
    await db.from('notes').insert([
      {'doc_id': 1, 'stars': 5},
      {'doc_id': 1, 'stars': 3},
      {'doc_id': 3, 'stars': 4},
    ]);
    final v = Float32List.fromList([0.1, 0.2, 0.3]);
    final pairs = [
      (
        shelf.select(['title']).where('year', '>=', 2022).order('year', 'desc'),
        'get shelf select title where year >= \$1 order year desc',
        [2022]
      ),
      (
        shelf.select(['title']).where({
          'lang': 'en',
          'tags': {'has': 'sand'}
        }),
        'get shelf select title where lang = \$1 and tags has \$2',
        ['en', 'sand']
      ),
      (
        shelf
            .select(['title'])
            .where(Cond.or([Cond.cmp('lang', '=', 'tr'), Cond.cmp('year', '<', 2022)]))
            .order('title'),
        'get shelf select title where lang = \$1 or year < \$2 order title asc',
        ['tr', 2022]
      ),
      (shelf.select(['title']).near('embed', v).limit(2), 'get shelf select title near embed \$1 limit 2', [v]),
      (shelf.select(['title']).match('body', 'oasis stars'), 'get shelf select title match body \$1', ['oasis stars']),
      (
        shelf.select(['title']).where('id', 'in', [1, 3]).lookup('notes',
            on: 'doc_id', select: ['stars'], order: [const SortKey('stars', 'desc')]),
        'get shelf select title where id in [\$1, \$2] lookup notes on doc_id select stars order stars desc',
        [1, 3]
      ),
      (
        shelf.select(['lang', 'count(*)']).group('lang').order('lang'),
        'get shelf select lang, count(*) group lang order lang asc',
        []
      ),
    ];
    for (final (q, text, params) in pairs) {
      expect(q.toFenecQL().text, text);
      final got = await q.rows();
      expect(got, isNotEmpty, reason: text);
      expect(got, await db.query(text, params), reason: text);
    }
    expect(await shelf.where('lang', 'en').count(), 2);
    expect((await shelf.order('year').first())?['title'], 'Dunes');
    expect(await shelf.near('embed', v).limit(1).explain(), isNotEmpty);
    expect(await shelf.where('lang', 'tr').update({'year': 2025}), 1);
    expect(() => shelf.delete(), throwsA(isA<FenecException>()));
    expect(await shelf.where('year', '<', 2022).delete(), 1);
    expect(await shelf.delete(all: true), 2);
    await db.close();
  });

  test('a query is a value to branch from', () {
    final b = Query.from('articles').where('year', '>=', 2024);
    expect(b.toFenecQL().text, 'get articles where year >= \$1');
    expect(b.where('tags', 'has', 'rust').toFenecQL().text, 'get articles where year >= \$1 and tags has \$2');
    expect(b.limit(3).toFenecQL().text, 'get articles where year >= \$1 limit 3');
    expect(b.reads, ['articles']);
    expect(b.lookup('notes', on: 'article_id').reads, ['articles', 'notes']);
    expect(b.where(Cond.raw('id in (get x select id)')).reads, isNull);
  });
}
