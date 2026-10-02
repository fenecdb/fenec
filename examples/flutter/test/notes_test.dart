import 'dart:io';

import 'package:flutter_test/flutter_test.dart';

import 'smoke.dart';

// The smoke steps under `flutter test`, on the machine running it: the
// library is the one FENEC_LIBRARY names (run-tests.sh builds it).
void main() {
  test('notes: seeds, search, filters, a live query, a reopen', () async {
    final dir = await Directory.systemTemp.createTemp('notes');
    addTearDown(() => dir.delete(recursive: true));
    await smoke(dir, File('schema.fenecql').readAsStringSync());
  });
}
