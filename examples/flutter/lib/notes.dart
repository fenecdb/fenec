// The Notes app's data: the schema, the seeds, the toy embedding and the
// queries. Only package:fenecdb, so the test runs it with no Flutter UI.
import 'dart:convert';
import 'dart:math';
import 'dart:typed_data';

import 'package:fenecdb/fenecdb.dart';

/// A placeholder for a real embedding model: hashed character trigrams
/// (of the UTF-8 bytes, ASCII lowered) into 64 dimensions, unit length.
/// It finds shared spellings, not meaning. A real app calls a model here --
/// TFLite or ONNX Runtime on the device, or an embeddings API -- and
/// declares `vector<N>` with that model's N.
Float32List embed(String text) {
  final lowered = text.replaceAllMapped(RegExp('[A-Z]'), (m) => m[0]!.toLowerCase());
  final bytes = utf8.encode(' $lowered ');
  final v = Float32List(64);
  for (var i = 0; i + 3 <= bytes.length; i++) {
    var h = 0x811c9dc5;
    for (var j = i; j < i + 3; j++) {
      h = ((h ^ bytes[j]) * 0x01000193) & 0xffffffff;
    }
    v[h % 64] += 1;
  }
  final norm = sqrt(v.fold<double>(0, (s, x) => s + x * x));
  if (norm > 0) {
    for (var i = 0; i < v.length; i++) {
      v[i] /= norm;
    }
  }
  return v;
}

const seeds = [
  ('Groceries', 'Buy milk, eggs and fresh bread for the weekend.', ['home', 'shopping'], false, '2026-09-28T09:00:00Z'),
  (
    'Release checklist',
    'Tag the release, publish the packages and update the docs.',
    ['work'],
    false,
    '2026-09-29T09:00:00Z',
  ),
  (
    'Book flights',
    'Find cheap flights to Istanbul for the conference in spring.',
    ['travel', 'work'],
    true,
    '2026-09-30T09:00:00Z',
  ),
  (
    'Book club',
    'Finish the novel about the desert fox before Thursday.',
    ['home', 'reading'],
    false,
    '2026-10-01T09:00:00Z',
  ),
];

/// What the list shows: every field but the vector.
const shown = ['id', 'title', 'body', 'tags', 'done', 'at'];

class Notes {
  final Fenec db;
  Notes(this.db);

  /// Opens the file, brings it to [schema] (schema.fenecql) and seeds an
  /// empty collection.
  static Future<Notes> open(String path, String schema) async {
    final db = await Fenec.open(path);
    await db.schema(schema);
    final notes = Notes(db);
    if (await db.from('notes').count() == 0) {
      for (final (title, body, tags, done, at) in seeds) {
        await notes.add(title, body, tags, done: done, at: DateTime.parse(at));
      }
    }
    return notes;
  }

  Future<int> add(String title, String body, List<String> tags, {bool done = false, DateTime? at}) =>
      db.from('notes').insert({
        'title': title,
        'body': body,
        'tags': tags,
        'done': done,
        'at': at ?? DateTime.now().toUtc(),
        'embed': embed('$title $body'),
      });

  Future<int> finish(int id) => db.from('notes').where('id', id).update({'done': true});

  /// Newest first, by a tag or the open ones only.
  Query list({String? tag, bool openOnly = false}) {
    var q = db.from('notes').select(shown);
    if (tag != null) q = q.where('tags', 'has', tag);
    if (openOnly) q = q.where('done', false);
    return q.order('at', 'desc').limit(20);
  }

  /// The words (BM25 over `body`) and the toy vector, fused by rank.
  Query search(String words) =>
      db.from('notes').select(shown).match('body', words).near('embed', embed(words)).fuse().limit(10);

  Query match(String words) => db.from('notes').select(shown).match('body', words).limit(10);

  Query similar(String words) => db.from('notes').select(shown).near('embed', embed(words)).limit(10);

  /// The rows of [query] now and after every write to notes.
  Stream<List<Map<String, Object?>>> live(Query query) => db.live(query);
}
