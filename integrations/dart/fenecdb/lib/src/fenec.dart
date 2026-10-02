import 'dart:async';
import 'dart:convert';
import 'dart:typed_data';

import 'live.dart';
import 'native.dart';
import 'query.dart';
import 'values.dart';

/// What the library or the builder refused, and why. [code] is the kind:
/// the engine's ([FenecCode.notFound], [FenecCode.duplicate] ...), the
/// boundary's ([FenecCode.misuse], [FenecCode.locked], [FenecCode.panic]),
/// or [FenecCode.builder] for a chain the query builder refused before
/// anything ran -- its message the JS builder's, word for word.
class FenecException implements Exception {
  final FenecCode code;
  final String message;

  /// The parameters the library asked for again as JSON (`exact`).
  final List<int>? exact;

  FenecException(this.code, this.message, [this.exact]);

  @override
  String toString() => 'FenecException(${code.name}): $message';
}

enum FenecCode {
  type(1),
  notFound(2),
  exists(3),
  duplicate(4),
  corrupt(5),
  query(6),
  io(7),
  plugin(8),
  readOnly(9),
  denied(10),
  panic(11),
  misuse(12),
  locked(13),
  builder(100);

  final int value;
  const FenecCode(this.value);

  static FenecCode of(int v) => values.firstWhere((c) => c.value == v, orElse: () => panic);
}

/// An answer to a statement: rows, a count of what a write wrote, a word
/// that it was done, or the schemas `collections` and `describe` give.
class Answer {
  final String kind;
  final List<String> columns;
  final List<Map<String, Object?>> rows;
  final int affected;
  final String message;
  final List<Map<String, Object?>> schemas;

  Answer._(this.kind,
      {this.columns = const [], this.rows = const [], this.affected = 0, this.message = '', this.schemas = const []});

  static Answer of(Map<String, Object?> v) => switch (v['kind']) {
        'rows' => Answer._('rows',
            columns: ((v['result'] as Map)['columns'] as List).cast<String>(),
            rows: ((v['result'] as Map)['rows'] as List).cast<Map<String, Object?>>()),
        'affected' => Answer._('affected', affected: v['count'] as int),
        'ok' => Answer._('ok', message: v['message'] as String),
        'schemas' => Answer._('schemas', schemas: (v['collections'] as List).cast<Map<String, Object?>>()),
        _ => throw FenecException(FenecCode.query, 'an answer of no kind the binding knows: $v'),
      };
}

/// What [Fenec.changes] answers: the counter now, and the collections
/// written since -- null when it cannot be told which, everything stale.
class Changes {
  final int seq;
  final int horizon;
  final List<String>? collections;
  Changes(this.seq, this.horizon, this.collections);
}

/// A fenecdb database in a file on the device -- or in memory -- through the
/// native library (crates/fenec-ffi): the engine and the file format of the
/// server and the browser.
///
/// ```dart
/// final dir = await getApplicationDocumentsDirectory();   // path_provider
/// final db = await Fenec.open('${dir.path}/app.fenec');
/// await db.execute('create collection todos (title text, done bool @hash)');
/// await db.from('todos').insert({'title': 'milk', 'done': false});
/// final open = await db.from('todos').where('done', false).rows();
/// ```
///
/// Every call runs on an isolate the database keeps for itself: a call may
/// wait for the lock, an fsync, a whole open, and an FFI call blocks the
/// isolate that makes it. Open a file once in a process -- a second open is
/// refused ([FenecCode.locked]), since two databases over one file corrupt
/// it.
class Fenec {
  /// Where the native library is, for a test or a desktop app: else
  /// `FENEC_LIBRARY`, else the platform's place (see the Flutter plugin).
  static String? library;

  /// Writes wait in a buffer for [sync], [flush] or [close] rather than an
  /// fsync each -- a few milliseconds a write on a phone.
  static const noSync = 1;

  /// The file read into memory rather than mapped: for a file whose pages
  /// may become unreadable while it is open, where a mapped page read would
  /// be the process's end.
  static const inMemory = 2;

  final int _handle;
  final Worker _worker;
  var _closed = false;

  /// Writes under way, which a live query's look waits out.
  var _inflight = 0;
  late final Lives lives = Lives(this);

  Fenec._(this._handle, this._worker);

  /// Opens the file at [path], made when missing. [flags]: [noSync],
  /// [inMemory].
  static Future<Fenec> open(String path, {int flags = 0}) async {
    final worker = await Worker.start(library);
    try {
      return Fenec._(int.parse(_answer(await worker.call('open', [path, flags]))), worker);
    } catch (_) {
      worker.stop();
      rethrow;
    }
  }

  /// A database in memory alone.
  static Future<Fenec> memory() async {
    final worker = await Worker.start(library);
    return Fenec._(int.parse(_answer(await worker.call('memory', []))), worker);
  }

  /// The library's version.
  static Future<String> version() async {
    final worker = await Worker.start(library);
    try {
      return _answer(await worker.call('version', []));
    } finally {
      worker.stop();
    }
  }

  static String _answer((int, String) r) {
    final (code, text) = r;
    if (code == 0) return text;
    final v = (() {
      try {
        return jsonDecode(text) as Map<String, Object?>;
      } catch (_) {
        return const <String, Object?>{};
      }
    })();
    throw FenecException(
        FenecCode.of(code), v['message'] as String? ?? 'error $code', (v['exact'] as List?)?.cast<int>());
  }

  // ------------------------------------------------------------- running

  /// Runs FenecQL -- one statement or several, which land together -- with
  /// [params] for `$1`, `$2` ...
  Future<Answer> run(String text, [List<Object?> params = const []]) => runQuiet(text, params, quiet: false);

  /// The rows a statement answers.
  Future<List<Map<String, Object?>>> query(String text, [List<Object?> params = const []]) async =>
      (await run(text, params)).rows;

  /// A write: how many documents it wrote.
  Future<int> execute(String text, [List<Object?> params = const []]) async => (await run(text, params)).affected;

  /// The query builder over a collection: `db.from('todos').where('done', false).rows()`.
  Query from(String collection) => Query.from(collection).bind(run);

  /// [run], the live queries' own runs not asking for a look.
  Future<Answer> runQuiet(String text, List<Object?> params, {required bool quiet}) async {
    if (_closed) throw FenecException(FenecCode.misuse, 'the database was closed');
    final values = params.map(normalize).toList();
    if (!quiet) _inflight++;
    try {
      try {
        return await _send(text, values, const {});
      } on FenecException catch (e) {
        // A json field keeps a list of numbers as written: those the
        // library asks for go again inside the JSON.
        if (e.exact == null) rethrow;
        return await _send(text, values, e.exact!.toSet());
      }
    } finally {
      if (!quiet) {
        _inflight--;
        // After an error too: a text that failed may follow statements
        // that wrote, and the change ring says what landed.
        lives.touch();
      }
    }
  }

  bool get writing => _inflight > 0;

  /// Runs [text]: the vectors among the parameters -- a [Float32List], or a
  /// list of finite numbers, which the engine reads as a vector either way
  /// -- go over as their bytes, their place null in the JSON, unless
  /// [asJson] names them. Written out as text and read back, a 128-dim
  /// vector's digits took a put from 4.6 us to 23.3 (`make ffi-bench`).
  Future<Answer> _send(String text, List<Object?> params, Set<int> asJson) async {
    final json = List<Object?>.of(params);
    final vectors = BytesBuilder(copy: false);
    for (var i = 0; i < params.length; i++) {
      if (asJson.contains(i)) continue;
      final f = vectorOf(params[i]);
      if (f == null) continue;
      json[i] = null;
      final b = ByteData(8 + 4 * f.length)
        ..setUint32(0, i, Endian.little)
        ..setUint32(4, f.length, Endian.little);
      for (var k = 0; k < f.length; k++) {
        // A -0 goes as 0, as JSON writes it: either way, one vector.
        b.setFloat32(8 + 4 * k, f[k] == 0 ? 0 : f[k], Endian.little);
      }
      vectors.add(b.buffer.asUint8List());
    }
    final out = _answer(
        await _worker.call('query', [_handle, text, writeJson(json), vectors.isEmpty ? null : vectors.takeBytes()]));
    return Answer.of(jsonDecode(out) as Map<String, Object?>);
  }

  // ------------------------------------------------------------- changes

  /// What changed since [since]: the counter now, and the collections written.
  Future<Changes> changes(int since) async {
    final v = jsonDecode(_answer(await _worker.call('changes', [_handle, since]))) as Map<String, Object?>;
    return Changes(v['seq'] as int, v['horizon'] as int, (v['collections'] as List?)?.cast<String>());
  }

  // ---------------------------------------------------------- durability

  /// Every write so far on disk: written and fsynced, the fsync with no lock held.
  Future<void> sync() async => _answer(await _worker.call('sync', [_handle]));

  /// Every write so far handed to the system, no fsync: it outlives the app
  /// being killed, not the device losing power -- microseconds, what an app
  /// does as it is paused.
  Future<void> flush() async => _answer(await _worker.call('flush', [_handle]));

  /// The file written anew as an image of the database, graphs and all: the
  /// next open links nothing.
  Future<void> checkpoint() async => _answer(await _worker.call('checkpoint', [_handle]));

  /// Saves the graphs, syncs and lets the file go; the live queries stop.
  Future<void> close() async {
    if (_closed) return;
    _closed = true;
    lives.clear();
    try {
      _answer(await _worker.call('close', [_handle]));
    } finally {
      _worker.stop();
    }
  }
}
