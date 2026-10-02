import 'dart:io';
import 'dart:typed_data';

import 'package:fenecdb/fenecdb.dart';
import 'package:test/test.dart';

/// The Dart tab of site/content/docs/languages.html, as it is written there,
/// and the builder lines below it.
void main() {
  test("the docs' example", () async {
    final path = '${Directory.systemTemp.createTempSync('fenecdb-docs').path}/app.fenec';
    final db = await Fenec.open(path);

    await db.execute('create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))');
    await db.execute(r'put docs {title: $1, embed: $2}', [
      'Night at the oasis',
      Float32List.fromList([0.1, 0.2, 0.3])
    ]);
    await db.execute(r'put docs {title: $1, embed: $2}', [
      'Dunes',
      Float32List.fromList([0.9, 0.1, 0.0])
    ]);

    final rows = await db.query(r'get docs select title near embed $1 limit 5', [
      Float32List.fromList([0.1, 0.2, 0.3])
    ]);
    expect(rows[0]['title'], 'Night at the oasis');
    expect(rows[0]['_score'], isA<num>());

    try {
      await db.query('get nowhere');
      fail('a collection that is not there answered');
    } on FenecException catch (e) {
      expect(e.code, FenecCode.notFound);
    }

    final docs = db.from('docs');
    await docs.insert({
      'title': 'Night at the oasis',
      'embed': Float32List.fromList([0.1, 0.2, 0.3])
    });
    final near = await docs.select(['title']).near('embed', Float32List.fromList([0.1, 0.2, 0.3])).limit(5).rows();
    final n = await docs.where('title', '~', 'Dunes').count();
    expect(near, hasLength(3));
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
