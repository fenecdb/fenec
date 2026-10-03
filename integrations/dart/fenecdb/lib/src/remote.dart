import 'dart:convert';
import 'dart:io';

import 'fenec.dart';
import 'query.dart';
import 'values.dart';

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
  String? token;
  final HttpClient _client = HttpClient()..connectionTimeout = const Duration(seconds: 10);

  FenecRemote(String url, {this.token}) : url = url.endsWith('/') ? url.substring(0, url.length - 1) : url;

  /// Runs FenecQL on the server with [params] for `$1`, `$2` ...
  Future<Answer> run(String text, [List<Object?> params = const []]) async {
    final int status;
    final String body;
    try {
      final req = await _client.postUrl(Uri.parse('$url/query'));
      req.headers.set('content-type', 'application/json');
      final t = token;
      if (t != null) req.headers.set('authorization', 'Bearer $t');
      // fenec-server reads a body by its length: a chunked one is refused.
      final bytes = utf8.encode(writeJson({'query': text, 'params': params.map(normalize).toList()}));
      req.contentLength = bytes.length;
      req.add(bytes);
      final res = await req.close();
      status = res.statusCode;
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
      final why = v is Map ? v['error'] as String? : null;
      throw FenecException(_code(status), why ?? 'HTTP $status');
    }
    return _answer(v);
  }

  /// The rows a statement answers.
  Future<List<Map<String, Object?>>> query(String text, [List<Object?> params = const []]) async =>
      (await run(text, params)).rows;

  /// A write: how many documents it wrote.
  Future<int> execute(String text, [List<Object?> params = const []]) async => (await run(text, params)).affected;

  /// The query builder, its queries run on the server.
  Query from(String collection) => Query.from(collection).bind(run);

  /// Lets the connections go.
  void close() => _client.close();

  /// An HTTP status as the error kind the engine would have said.
  static FenecCode _code(int status) => switch (status) {
        400 => FenecCode.query,
        401 || 403 => FenecCode.denied,
        404 => FenecCode.notFound,
        409 => FenecCode.duplicate,
        _ => FenecCode.io,
      };

  /// The endpoint's answer as the library's: rows come as an array, or --
  /// when the query asked for facets, which belong to no row -- as an
  /// object holding them beside the rows.
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
