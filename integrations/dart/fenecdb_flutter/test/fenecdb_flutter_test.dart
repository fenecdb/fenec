import 'package:fenecdb_flutter/fenecdb_flutter.dart';
import 'package:flutter_test/flutter_test.dart';

// The plugin under `flutter test`, on the machine running it: the library
// is the one FENEC_LIBRARY names, as the fenecdb package's own tests take
// it -- what a device bundles is built by `flutter build`.
void main() {
  test('a database through the plugin, and a live query', () async {
    final db = await Fenec.memory();
    await db.execute('create collection todos (title text, done bool @hash)');
    final seen = <int>[];
    final sub = db.live(db.from('todos').where('done', false)).listen((rows) => seen.add(rows.length));
    await db.from('todos').insert({'title': 'milk', 'done': false});
    for (var i = 0; i < 200 && (seen.isEmpty || seen.last != 1); i++) {
      await Future<void>.delayed(const Duration(milliseconds: 10));
    }
    expect(seen.last, 1);
    await sub.cancel();
    await db.close();
  });
}
