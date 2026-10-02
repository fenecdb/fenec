import 'dart:io';

import 'package:fenecdb/fenecdb.dart';
import 'package:test/test.dart';

import '../../../types-golden/fenec_schema.dart' as generated;

/// A schema written as FenecQL, through the native library: made, checked
/// again, refused when it would lose data, and migrated once.
void main() {
  test('a schema is made, checked and migrated', () async {
    final file = '${Directory.systemTemp.createTempSync('fenecdb-dart-schema').path}/app.fenec';
    const v1 = 'create collection notes (title text required, at timestamp @sorted)';

    final db = await Fenec.open(file);
    final made = await db.schema(v1);
    expect(made.applied, isTrue);
    expect(made.statements, [v1]);
    await db.execute('put notes {title: "a"}');
    await db.close();

    // Opened again with the same schema: nothing to do.
    final again = await Fenec.open(file);
    final nothing = await again.schema(v1);
    expect([nothing.applied, nothing.statements, nothing.migrations], [false, isEmpty, isEmpty]);
    // A rename unsaid could be a drop and an add: refused, nothing written.
    const v2 = 'create collection notes (name text required, at timestamp @sorted)';
    await expectLater(
      again.schema(v2),
      throwsA(
          isA<SchemaException>().having((e) => e.refusals.map((r) => r.kind), 'kinds', contains('field_not_declared'))),
    );
    // Said, as a migration: run once, recorded.
    const moved = ['alter collection notes rename field title to name'];
    final migrated = await again.schema(v2, migrations: moved);
    expect([
      migrated.ran,
      migrated.migrations
    ], [
      true,
      [1]
    ]);
    expect((await again.query('get notes select name')).map((r) => r['name']), ['a']);
    expect((await again.schema(v2, migrations: moved)).migrations, isEmpty);
    // A plan writes nothing.
    final plan = await again.schema('$v2\ncreate collection tags (name text)', migrations: moved, apply: false);
    expect([
      plan.applied,
      plan.statements
    ], [
      false,
      ['create collection tags (name text)']
    ]);
    await again.close();
  });

  test('what fenec types --lang dart writes reads a row the binding answers', () async {
    final db = await Fenec.memory();
    await db
        .schema('create collection product_reviews (product_id int required @hash, stars int, notes text collate und)');
    await db.execute('put product_reviews {product_id: 7, notes: "fine"}');
    final r = generated.ProductReviews.fromJson((await db.query('get product_reviews'))[0]);
    expect([r.id, r.productId, r.stars, r.notes], [1, 7, null, 'fine']);
    await db.close();
  });
}
