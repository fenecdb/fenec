import 'dart:io';

import 'package:fenecdb/fenecdb.dart';
import 'package:test/test.dart';

/// The Dart tab of site/content/docs/languages.html, as it is written there,
/// and the builder lines below it.
void main() {
  test("the docs' example", () async {
    final path = '${Directory.systemTemp.createTempSync('fenecdb-docs').path}/app.fenec';
    final db = await Fenec.open(path);

    await db.execute('create collection if not exists docs (title text @text, year int @sorted)');
    await db.execute(r'put docs {title: $1, year: $2}', ['Dunes', 2021]);
    await db.execute(r'put docs {title: $1, year: $2}', ['Night at the oasis', 2024]);

    final rows = await db.query(r'get docs select title, year where year >= $1 order year desc limit $2', [2020, 5]);
    expect([for (final r in rows) r['title']], ['Night at the oasis', 'Dunes']);
    expect(rows[0]['year'], 2024);
    final hits = await db.query(r'get docs select title match title $1 fuzzy', ['oasys']);
    expect([for (final r in hits) r['title']], ['Night at the oasis']);
    expect(hits[0]['_score'], greaterThan(0));

    try {
      await db.query('get nowhere');
      fail('a collection that is not there answered');
    } on FenecException catch (e) {
      expect(e.code, FenecCode.notFound);
    }

    final docs = db.from('docs');
    await docs.insert({'title': 'Night at the oasis', 'year': 2024});
    final recent = await docs.select(['title', 'year']).where('year', '>=', 2020).order('year', 'desc').limit(5).rows();
    final found = await docs.select(['title']).match('title', 'oasys', fuzzy: true).rows();
    final n = await docs.where('title', '~', 'Dunes').count();
    expect([for (final r in recent) r['title']], ['Night at the oasis', 'Night at the oasis', 'Dunes']);
    expect(found, hasLength(2));
    expect(n, 1);
    // The builder table's condition.
    expect(
        docs
            .where(Cond.or([
              {'lang': 'tr'},
              {
                'tags': {'has': 'rust'}
              }
            ]))
            .toFenecQL()
            .text,
        r'get docs where lang = $1 or tags has $2');
    await db.close();
  });
}
