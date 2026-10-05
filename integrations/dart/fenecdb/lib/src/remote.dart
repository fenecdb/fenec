import 'dart:convert';
import 'dart:io';

import 'fenec.dart';
import 'query.dart';
import 'values.dart';

/// What [FenecRemote.batch] answers: each statement's answer in order, the
/// change the batch left the database at, and whether the answer is the one
/// kept for its idempotency key ([replayed]) -- a replayed answer carries
/// no [seq].
class BatchAnswer {
  final List<Answer> results;
  final int? seq;
  final bool replayed;

  const BatchAnswer(this.results, this.seq, this.replayed);
}

/// What a connection and the copies [FenecRemote.withIdempotencyKey] makes
/// of it share, as Go's and .NET's copies share theirs: the connections, the
/// token and the last `Fenec-Seq`.
class _Shared {
  final HttpClient client = HttpClient()..connectionTimeout = const Duration(seconds: 10);
  String? token;
  int? seq;

  _Shared(this.token);
}

/// A fenec-server over HTTP, with no file on the device: every query goes
/// to the server (`POST /query`), through the same builder. What
/// [Fenec.connect] makes, as `connect` does in JS.
///
/// ```dart
/// final db = Fenec.connect('https://api.example.com', token: jwt);
/// final open = await db.from('todos').where('done', false).rows();
/// ```
///
/// The requests are `dart:io`'s `HttpClient`'s, so TLS is the platform's.
class FenecRemote {
  final String url;
  final _Shared _shared;
  final String? _idempotencyKey;

  FenecRemote(String url, {String? token})
      : url = url.endsWith('/') ? url.substring(0, url.length - 1) : url,
        _shared = _Shared(token),
        _idempotencyKey = null;

  FenecRemote._keyed(FenecRemote of, String key)
      : url = of.url,
        _shared = of._shared,
        _idempotencyKey = key;

  /// The token the requests carry; set a fresh one for the requests from
  /// here on.
  String? get token => _shared.token;
  set token(String? t) => _shared.token = t;

  /// The change the last write through this connection, or a copy of it,
  /// left the database at (`Fenec-Seq`); null before any.
  int? get seq => _shared.seq;

  /// A copy whose writes -- [run], [batch] and the builder's -- carry [key]
  /// as their `Idempotency-Key`: sent again after a timeout, a write is
  /// answered as it was the first time and not made twice. One key a write:
  /// the same key with another request is refused (status 422). It shares
  /// this connection's token, `seq` and connections.
  ///
  /// ```dart
  /// await db.withIdempotencyKey(orderId).from('orders').insert(order);
  /// ```
  FenecRemote withIdempotencyKey(String key) {
    if (key.isEmpty) throw FenecException(FenecCode.misuse, 'an idempotency key is a text, not empty');
    return FenecRemote._keyed(this, key);
  }

  /// Runs FenecQL on the server with [params] for `$1`, `$2` ... A key for
  /// one write is [withIdempotencyKey]'s: Dart takes no named parameter
  /// beside the optional [params].
  Future<Answer> run(String text, [List<Object?> params = const []]) async {
    final (v, _, _) = await _post(
        '/query', 'application/json', writeJson({'query': text, 'params': params.map(normalize).toList()}), null);
    return _answer(v);
  }

  /// `POST /batch`: the statements in order under one write lock, as one
  /// block -- their writes all land, or at the first error none of them do,
  /// a [FenecException] whose [FenecException.at] is the statement that
  /// stopped it ([FenecCode.unmet] for a write's `require` not met). A
  /// statement is what the builder's `toInsert`, `toUpdate`, `toDelete` and
  /// `toFenecQL` make. With [idempotencyKey] a retry after a timeout is
  /// answered as the first try was and writes nothing twice; a batch of
  /// reads alone takes no key.
  Future<BatchAnswer> batch(List<Statement> statements, {String? idempotencyKey}) async {
    final lines = [
      for (final s in statements) writeJson({'query': s.text, 'params': s.params.map(normalize).toList()})
    ];
    final (v, seq, replayed) = await _post('/batch', 'application/x-ndjson', lines.join('\n'), idempotencyKey);
    final results = v is Map && v['results'] is List ? (v['results'] as List).map(_answer).toList() : <Answer>[];
    return BatchAnswer(results, seq, replayed);
  }

  /// A POST's JSON answer, its `Fenec-Seq` and whether it was replayed; a
  /// refusal as a [FenecException] with its status and, for a batch, where
  /// it stopped.
  Future<(Object?, int?, bool)> _post(String path, String type, String payload, String? key) async {
    final int status;
    final String body;
    final HttpHeaders headers;
    try {
      final req = await _shared.client.postUrl(Uri.parse('$url$path'));
      req.headers.set('content-type', type);
      final t = _shared.token;
      if (t != null) req.headers.set('authorization', 'Bearer $t');
      final k = key ?? _idempotencyKey;
      if (k != null) req.headers.set('idempotency-key', k);
      // fenec-server reads a body by its length: a chunked one is refused.
      final bytes = utf8.encode(payload);
      req.contentLength = bytes.length;
      req.add(bytes);
      final res = await req.close();
      status = res.statusCode;
      headers = res.headers;
      body = await res.transform(utf8.decoder).join();
    } on IOException catch (e) {
      throw FenecException(FenecCode.io, 'the server could not be reached: $e');
    }
    final Object? v;
    try {
      v = body.isEmpty ? null : jsonDecode(body);
    } catch (_) {
      throw FenecException(FenecCode.io, 'the server did not answer JSON ($status)');
    }
    if (status < 200 || status > 299) {
      final m = v is Map ? v : null;
      throw FenecException.http(_code(status), m?['error'] as String? ?? 'HTTP $status',
          status: status, at: m?['at'] as int?, completed: m?['completed'] as int?);
    }
    final seq = int.tryParse(headers.value('fenec-seq') ?? '');
    if (seq != null) _shared.seq = seq;
    return (v, seq, headers.value('idempotent-replayed') == 'true');
  }

  /// The rows a statement answers.
  Future<List<Map<String, Object?>>> query(String text, [List<Object?> params = const []]) async =>
      (await run(text, params)).rows;

  /// A write: how many documents it wrote.
  Future<int> execute(String text, [List<Object?> params = const []]) async => (await run(text, params)).affected;

  /// The query builder, its queries run on the server.
  Query from(String collection) => Query.from(collection).bind(run);

  /// Lets the connections go: this one's, and every copy's.
  void close() => _shared.client.close();

  /// An HTTP status as the error kind the engine would have said; the
  /// status itself is the error's [FenecException.status].
  static FenecCode _code(int status) => switch (status) {
        400 => FenecCode.query,
        401 || 403 => FenecCode.denied,
        404 => FenecCode.notFound,
        409 => FenecCode.duplicate,
        412 => FenecCode.unmet,
        _ => FenecCode.io,
      };

  /// The endpoint's answer as the library's: rows come as an array, or --
  /// when the query asked for facets, which belong to no row, and as a
  /// batch's each read -- as an object holding them beside the rows.
  static Answer _answer(Object? v) {
    if (v is List || (v is Map && v['rows'] is List)) {
      final rows = ((v is List ? v : (v as Map)['rows']) as List).cast<Map<String, Object?>>();
      return Answer.of({
        'kind': 'rows',
        'result': {
          'columns': rows.isEmpty ? <String>[] : rows.first.keys.toList(),
          'rows': rows,
          if (v is Map) 'facets': v['facets'],
        },
      });
    }
    if (v is Map<String, Object?>) {
      if (v['affected'] is int) return Answer.of({'kind': 'affected', 'count': v['affected']});
      if (v['collections'] is List) return Answer.of({'kind': 'schemas', 'collections': v['collections']});
      return Answer.of({'kind': 'ok', 'message': v['message'] as String? ?? jsonEncode(v)});
    }
    return Answer.of({'kind': 'ok', 'message': jsonEncode(v)});
  }
}
