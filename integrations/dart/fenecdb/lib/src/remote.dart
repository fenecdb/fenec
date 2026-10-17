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

/// Where a connection's reads go. [home], the default: every request to the
/// tenant's home. [nearest]: the builder's reads, and a
/// [FenecRemote.asRead] copy's requests, are marked (`Fenec-Read: replica`)
/// so that a fenec-shard router near a region holding the tenant's read
/// replica answers them there, each sent after the furthest position an
/// answer from home named to this connection (`Fenec-After`,
/// `Fenec-History`: the change and its history) -- never older than its own
/// writes, nor a write a failover replaced. Writes go home.
enum Reads { home, nearest }

/// Where an answer from home left the tenant: its change (`Fenec-Seq`) and
/// the history that change is of (`Fenec-History`, `<term>-<id>`; none for a
/// database that never had a replica).
class _Position {
  final int seq;
  final String? history;

  const _Position(this.seq, this.history);

  /// The history's term, one more at each promotion.
  int get term {
    final h = history;
    if (h == null) return 0;
    final dash = h.indexOf('-');
    return int.tryParse(dash < 0 ? h : h.substring(0, dash)) ?? 0;
  }

  /// The history's lineage: its id's top half, which every fork of the
  /// tenant keeps and a tenant made again under its name does not.
  String? get lineage {
    final h = history;
    final dash = h?.indexOf('-') ?? -1;
    return h == null || dash < 0 || h.length < dash + 9 ? null : h.substring(dash + 1, dash + 9);
  }

  /// Whether this is of another lineage than [q]: a tenant made again.
  bool isOfAnotherLineage(_Position q) {
    final a = lineage, b = q.lineage;
    return a != null && b != null && a != b;
  }

  /// Whether this is past [q]: of another lineage -- home's word on a tenant
  /// made again, whose terms start again -- or a later term of the same, or
  /// further in one.
  bool isPast(_Position? q) => q == null || isOfAnotherLineage(q) || term > q.term || (term == q.term && seq > q.seq);
}

/// What a connection and the copies [FenecRemote.withIdempotencyKey],
/// [FenecRemote.withWait] and [FenecRemote.asRead] make of it share, as Go's
/// and .NET's copies share theirs: the connections, the token and the
/// furthest position.
class _Shared {
  final HttpClient client = HttpClient()..connectionTimeout = const Duration(seconds: 10);
  final Reads reads;
  String? token;

  /// The furthest position an answer from home named, which a read near is
  /// sent after: an answer that arrives late is not the last.
  _Position? at;

  _Shared(this.token, this.reads);
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

  /// How long the server may hold a write that writes nothing ([withWait]).
  final Duration? _wait;

  /// This copy's requests only read ([asRead], the builder's reads).
  final bool _reading;

  FenecRemote(String url, {String? token, Reads reads = Reads.home})
      : url = url.endsWith('/') ? url.substring(0, url.length - 1) : url,
        _shared = _Shared(token, reads),
        _idempotencyKey = null,
        _wait = null,
        _reading = false;

  FenecRemote._copy(FenecRemote of, String? key, Duration? wait, bool reading)
      : url = of.url,
        _shared = of._shared,
        _idempotencyKey = key,
        _wait = wait,
        _reading = reading;

  /// Where this connection's reads go ([Fenec.connect]'s `reads`).
  Reads get reads => _shared.reads;

  /// The token the requests carry; set a fresh one for the requests from
  /// here on.
  String? get token => _shared.token;
  set token(String? t) => _shared.token = t;

  /// The furthest change an answer from home -- a write's, a read-only
  /// batch's -- left the database at (`Fenec-Seq`), through this connection
  /// or a copy of it; null before any. Answers arriving out of order do not
  /// move it back, a read near's answer, which may be a replica's, moves it
  /// not, and a read near's 504 sets it to where home says it stands, as
  /// Go's `Seq()` and .NET's `Seq` are.
  int? get seq => _shared.at?.seq;

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
    return FenecRemote._copy(this, key, _wait, _reading);
  }

  /// A copy whose `set` and `del` statements -- [run], [batch] and the
  /// builder's `update`, `delete` and their `Returning` forms -- the server
  /// holds while they write nothing, until they can or for [wait], at most
  /// 30 seconds (`Fenec-Wait`): a worker's claim waits there for a job --
  /// one enqueued, a delayed one come due, a lease lapsed -- rather than
  /// sleeping and claiming again.
  ///
  /// ```dart
  /// final claimed = await db.withWait(const Duration(seconds: 30)).from('jobs')
  ///     .where(Cond.raw('run_at <= now()')).order('run_at').limit(10).updateReturning(claim);
  /// ```
  FenecRemote withWait(Duration wait) {
    if (wait.isNegative) throw FenecException(FenecCode.misuse, 'a wait is not negative');
    return FenecRemote._copy(this, _idempotencyKey, wait, _reading);
  }

  /// A copy whose requests only read: on a connection made with `reads:
  /// Reads.nearest` a router near the tenant's read replica answers them
  /// there, after this connection's writes. The builder's reads go so on
  /// their own; a text the caller knows reads -- a [run], a [batch] of
  /// `toFenecQL()`s -- goes through this, and one that writes is refused
  /// (status 403), never landed. On a connection reading home it changes
  /// nothing. It shares this connection's token, `seq` and connections.
  ///
  /// ```dart
  /// final n = await db.asRead().query(r'get orders where customer = $1 count', ['c-7']);
  /// ```
  FenecRemote asRead() => FenecRemote._copy(this, _idempotencyKey, _wait, true);

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
    // A read near: marked for the tenant's read replica in the router's
    // region, after the furthest position home named to this connection,
    // which the replica waits for -- that change of that history.
    var near = _reading && _shared.reads == Reads.nearest;
    // A position of change 0 is none: nothing to wait for, as Go and .NET
    // send none.
    final at = _shared.at;
    final sent = near && at != null && at.seq > 0 ? at : null;
    var (status, body, headers) = await _send(path, type, payload, key, near, sent);
    // 504: no copy holds that write in the wait -- home had none either,
    // lost in a failover. Home says where it stands, which the connection
    // takes as its position, lower or not; asked home again with no mark,
    // the read is answered with what home holds. Taken only over the
    // position this request was sent after -- a write answered meanwhile is
    // further, and set back the next read near would miss it -- and never
    // of an earlier term of the same lineage: a router whose copy of the
    // directory lags a failover sends the read to the old primary, fenced,
    // which names the branch it abandoned. Another lineage's is a tenant
    // made again under the name, which starts again at term 1: refused,
    // every read near waited its whole wait for good.
    if (near && sent != null && status == 504) {
      final home = _decoded(body);
      if (home is Map && home['seq'] is int) {
        final history = home['history'];
        final p = _Position(home['seq'] as int, history is String ? history : null);
        if (identical(_shared.at, sent) && (p.term >= sent.term || p.isOfAnotherLineage(sent))) _shared.at = p;
      }
      near = false;
      (status, body, headers) = await _send(path, type, payload, key, false, null);
    }
    final Object? v;
    try {
      v = body.isEmpty ? null : jsonDecode(body);
    } catch (_) {
      throw FenecException(FenecCode.io, 'the server did not answer JSON ($status)');
    }
    if (status < 200 || status > 299) {
      throw refused(status, v is Map ? v : null, headers.value('retry-after'));
    }
    final seq = int.tryParse(headers.value('fenec-seq') ?? '');
    // A position from home alone: a read near's answer may be a replica's,
    // behind home or on a branch a failover left.
    if (seq != null && !near) {
      final p = _Position(seq, headers.value('fenec-history'));
      if (p.isPast(_shared.at)) _shared.at = p;
    }
    return (v, seq, headers.value('idempotent-replayed') == 'true');
  }

  /// The request sent -- marked as a read near after [after] where [near] --
  /// and its answer's status, body and head.
  Future<(int, String, HttpHeaders)> _send(
      String path, String type, String payload, String? key, bool near, _Position? after) async {
    try {
      final req = await _shared.client.postUrl(Uri.parse('$url$path'));
      req.headers.set('content-type', type);
      final t = _shared.token;
      if (t != null) req.headers.set('authorization', 'Bearer $t');
      final k = key ?? _idempotencyKey;
      if (k != null) req.headers.set('idempotency-key', k);
      final w = _wait;
      if (w != null && w > Duration.zero) req.headers.set('fenec-wait', '${w.inMilliseconds}');
      if (near) {
        req.headers.set('fenec-read', 'replica');
        if (after != null) {
          req.headers.set('fenec-after', '${after.seq}');
          final h = after.history;
          if (h != null) req.headers.set('fenec-history', h);
        }
      }
      // fenec-server reads a body by its length: a chunked one is refused.
      final bytes = utf8.encode(payload);
      req.contentLength = bytes.length;
      req.add(bytes);
      final res = await req.close();
      return (res.statusCode, await res.transform(utf8.decoder).join(), res.headers);
    } on IOException catch (e) {
      throw FenecException(FenecCode.io, 'the server could not be reached: $e');
    }
  }

  static Object? _decoded(String body) {
    try {
      return jsonDecode(body);
    } catch (_) {
      return null;
    }
  }

  /// The rows a statement answers.
  Future<List<Map<String, Object?>>> query(String text, [List<Object?> params = const []]) async =>
      (await run(text, params)).rows;

  /// A write: how many documents it wrote.
  Future<int> execute(String text, [List<Object?> params = const []]) async => (await run(text, params)).affected;

  /// The query builder, its queries run on the server -- its reads as an
  /// [asRead] copy's, near on a connection reading near.
  Query from(String collection) => Query.from(collection).bind(run, read: asRead().run);

  /// Lets the connections go: this one's, and every copy's.
  void close() => _shared.client.close();

  /// A refusal as the error it is: its status and message, where a batch
  /// stopped, and where a tenant's quota refused it the quota and how long
  /// to wait -- to the millisecond where the body says it
  /// (`retry_after_ms`), else `Retry-After`, or the same in the body.
  static FenecException refused(int status, Map? m, String? retryAfter) {
    final quota = m?['quota'] as String?;
    final ms = (m?['retry_after_ms'] as num?)?.toInt();
    final secs = int.tryParse(retryAfter ?? '') ?? (m?['retry_after'] as num?)?.toInt();
    final wait = ms != null && ms > 0
        ? Duration(milliseconds: ms)
        : secs == null
            ? null
            : Duration(seconds: secs);
    return FenecException.http(
        quota == null ? _code(status) : FenecCode.quota, m?['error'] as String? ?? 'HTTP $status',
        status: status, at: m?['at'] as int?, completed: m?['completed'] as int?, quota: quota, retryAfter: wait);
  }

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
