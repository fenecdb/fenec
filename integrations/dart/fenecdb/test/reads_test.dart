// Reads near a tenant's read replica (`Fenec.connect(url, reads:
// Reads.nearest)`): against real routers and nodes -- the binaries `cargo
// build -p fenec-server -p fenec-shard` makes (FENEC_SERVER and FENEC_SHARD
// name others), as `make dart-test` builds them -- and what a connection
// sends after its answers against a script: a failover's 504, answers
// arriving out of order, a tenant made again.
import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:fenecdb/fenecdb.dart';
import 'package:test/test.dart';

/// A server binary started on a port of its own, found by the address its
/// `listening on:` line names.
class Daemon {
  final Process process;
  final int port;

  Daemon._(this.process, this.port);

  static String binary(String name, String env) =>
      Platform.environment[env] ?? '${Directory.current.path}/../../../target/debug/$name';

  static Future<Daemon> start(String binary, List<String> args) async {
    final p = await Process.start(binary, args);
    p.stdout.drain<void>();
    final found = Completer<int>();
    final seen = StringBuffer();
    p.stderr.transform(utf8.decoder).transform(const LineSplitter()).listen((line) {
      seen.writeln(line);
      final m = RegExp(r'listening on: http://127\.0\.0\.1:(\d+) ').firstMatch(line);
      if (m != null && !found.isCompleted) found.complete(int.parse(m.group(1)!));
    }, onDone: () {
      if (!found.isCompleted) found.completeError(StateError('$binary did not start: $seen'));
    });
    try {
      return Daemon._(p, await found.future.timeout(const Duration(seconds: 20)));
    } catch (_) {
      p.kill(ProcessSignal.sigkill);
      rethrow;
    }
  }

  /// Killed, as a crash would; returns once it is gone.
  Future<void> kill() async {
    process.kill(ProcessSignal.sigkill);
    await process.exitCode;
  }
}

/// eu1 in eu and us1 in us, the global router near eu and its standby near
/// us; tenant `acme` on eu1 with its read replica on us1. A client in us
/// asks the router there: [near].
class ReadReplicas {
  final Daemon eu1;
  final Daemon us1;
  final List<Daemon> routers;
  final Directory dir;

  ReadReplicas._(this.eu1, this.us1, this.routers, this.dir);

  /// us's router.
  String get near => 'http://127.0.0.1:${routers[1].port}';

  static Future<ReadReplicas> start() async {
    final dir = await Directory.systemTemp.createTemp('fenecdb-dart-replicas');
    final server = Daemon.binary('fenec-server', 'FENEC_SERVER');
    final shard = Daemon.binary('fenec-shard', 'FENEC_SHARD');
    final started = <Daemon>[];
    try {
      Future<Daemon> node(String n) async {
        final d = await Daemon.start(server, [
          '--dir',
          '${dir.path}/$n',
          '--http',
          '127.0.0.1:0',
          '--admin-token',
          'adm',
          '--replication-token',
          'repl',
        ]);
        started.add(d);
        return d;
      }

      Future<Daemon> router(String file, List<String> extra) async {
        final d = await Daemon.start(shard, [
          '--listen',
          '127.0.0.1:0',
          '--directory',
          '${dir.path}/$file',
          '--token',
          'root',
          '--replication-token',
          'dirs',
          ...extra,
        ]);
        started.add(d);
        return d;
      }

      final eu1 = await node('eu1');
      final us1 = await node('us1');
      final eu = await router('global.fenec', ['--near', 'eu']);
      final us = await router('standby.fenec', ['--near', 'us', '--replica-of', 'http://127.0.0.1:${eu.port}']);
      final global = 'http://127.0.0.1:${eu.port}';
      await _call(
          global, 'PUT', '/_shard/nodes/eu1', {'addr': '127.0.0.1:${eu1.port}', 'token': 'adm', 'region': 'eu'});
      await _call(
          global, 'PUT', '/_shard/nodes/us1', {'addr': '127.0.0.1:${us1.port}', 'token': 'adm', 'region': 'us'});
      await _call(
          global, 'PUT', '/_shard/tenants/acme', {'node': 'eu1', 'schema': 'create collection notes (body text)'});
      final placed = await _call(global, 'PUT', '/_shard/tenants/acme/read-replicas/us', null);
      expect(placed['node'], 'us1');
      // us's router follows the global directory: it routes to the replica
      // once its copy holds it.
      await eventually("us's router knows of the read replica", () async {
        final list = await _call('http://127.0.0.1:${us.port}', 'GET', '/_shard/tenants/acme/read-replicas', null);
        return (list['read_replicas'] as List).any((r) => r['node'] == 'us1');
      });
      return ReadReplicas._(eu1, us1, [eu, us], dir);
    } catch (_) {
      for (final d in started) {
        await d.kill();
      }
      await dir.delete(recursive: true);
      rethrow;
    }
  }

  /// The rows the replica on us1 holds, asked of its node directly.
  Future<int?> replicaCount() async {
    final http = HttpClient();
    try {
      final req = await http.postUrl(Uri.parse('http://127.0.0.1:${us1.port}/t/acme/query'));
      req.headers.set('fenec-read', 'replica');
      _send(req, {'query': 'get notes count'});
      final res = await req.close();
      final body = await res.transform(utf8.decoder).join();
      if (res.statusCode != 200) return null;
      return ((jsonDecode(body) as List).first as Map)['count'] as int?;
    } finally {
      http.close();
    }
  }

  /// Everything killed, the files let go of.
  Future<void> stop() async {
    for (final d in [...routers, us1, eu1]) {
      await d.kill();
    }
    await dir.delete(recursive: true);
  }

  /// A router's admin call under its token: the JSON it answers.
  static Future<Map<String, Object?>> _call(String base, String method, String path, Map<String, String>? body) async {
    final http = HttpClient();
    try {
      final req = await http.openUrl(method, Uri.parse('$base$path'));
      req.headers.set('authorization', 'Bearer root');
      if (body != null) _send(req, body);
      final res = await req.close();
      final text = await res.transform(utf8.decoder).join();
      if (res.statusCode >= 300) throw StateError('$method $path: ${res.statusCode} $text');
      return text.isEmpty ? {} : jsonDecode(text) as Map<String, Object?>;
    } finally {
      http.close();
    }
  }

  /// [body] as JSON, its length said: the servers refuse a chunked body.
  static void _send(HttpClientRequest req, Object body) {
    final bytes = utf8.encode(jsonEncode(body));
    req.contentLength = bytes.length;
    req.add(bytes);
  }
}

/// One answer of a script: its status, body and head, given once [gate]
/// completes where it has one -- two requests under way at once.
class Scripted {
  final int status;
  final String body;
  final String? seq;
  final String? history;
  final Future<void>? gate;

  const Scripted({this.status = 200, this.body = '[]', this.seq, this.history, this.gate});
}

/// A server between a connection and where its requests go, keeping each
/// request's head in the order they came, as it arrives: passed on to
/// [target], or -- with none -- answered from [script] in turn, `[]` once
/// it runs out.
class Wire {
  final HttpServer _server;
  final String? target;
  final List<Scripted> script;
  final HttpClient _client = HttpClient();

  /// Every request's head so far.
  final List<Map<String, String>> heads = [];

  Wire._(this._server, this.target, this.script);

  static Future<Wire> start({String? target, List<Scripted>? script}) async {
    final w = Wire._(await HttpServer.bind('127.0.0.1', 0), target, script ?? []);
    w._server.listen(w._take);
    return w;
  }

  String get url => 'http://127.0.0.1:${_server.port}';

  /// The last request's head.
  Map<String, String> get last => heads.last;

  static const _hop = {'host', 'content-length', 'connection', 'transfer-encoding'};

  Future<void> _take(HttpRequest req) async {
    final head = <String, String>{};
    req.headers.forEach((name, values) => head[name.toLowerCase()] = values.join(','));
    heads.add(head);
    final body = await req.fold<List<int>>([], (all, chunk) => all..addAll(chunk));
    final res = req.response;
    final to = target;
    if (to == null) {
      final a = script.isEmpty ? const Scripted() : script.removeAt(0);
      await a.gate;
      res.statusCode = a.status;
      res.headers.contentType = ContentType.json;
      if (a.seq != null) res.headers.set('fenec-seq', a.seq!);
      if (a.history != null) res.headers.set('fenec-history', a.history!);
      res.write(a.body);
      await res.close();
      return;
    }
    final out = await _client.openUrl(req.method, Uri.parse('$to${req.uri}'));
    req.headers.forEach((name, values) {
      if (!_hop.contains(name)) {
        for (final v in values) {
          out.headers.add(name, v);
        }
      }
    });
    out.contentLength = body.length;
    out.add(body);
    final back = await out.close();
    final bytes = await back.fold<List<int>>([], (all, chunk) => all..addAll(chunk));
    res.statusCode = back.statusCode;
    back.headers.forEach((name, values) {
      if (!_hop.contains(name)) {
        for (final v in values) {
          res.headers.add(name, v);
        }
      }
    });
    res.contentLength = bytes.length;
    res.add(bytes);
    await res.close();
  }

  Future<void> close() async {
    _client.close(force: true);
    await _server.close(force: true);
  }
}

/// Waits until [done] holds, or fails after ten seconds.
Future<void> eventually(String what, FutureOr<bool> Function() done) async {
  for (var i = 0; i < 1000; i++) {
    if (await done()) return;
    await Future<void>.delayed(const Duration(milliseconds: 10));
  }
  fail('timed out waiting for $what');
}

const h1 = '1-00000000000000aa';
const h2 = '2-00000000000000bb';

Scripted written(String seq, String history) => Scripted(body: '{"affected":1}', seq: seq, history: history);

Scripted refused(int seq, String history, {Future<void>? gate}) =>
    Scripted(status: 504, body: '{"error":"not in this copy","seq":$seq,"history":"$history"}', gate: gate);

Future<int> status(Future<Object?> Function() call) async {
  try {
    await call();
  } on FenecException catch (e) {
    return e.status ?? -1;
  }
  return 200;
}

void main() {
  // The builder's reads, and an `asRead()` copy's, are answered by the
  // replica near the app -- shown by an answer while home cannot be
  // reached, which only the replica could give -- and each is sent after
  // the connection's last write, whichever copy wrote it, so it never reads
  // older than what it wrote; writes, and every request of a connection
  // reading home, go home.
  test('reads near a read replica are answered there, never older than the app wrote', () async {
    final w = await ReadReplicas.start();
    final sent = await Wire.start(target: w.near);
    final db = Fenec.connect('${sent.url}/t/acme', reads: Reads.nearest);
    final home = Fenec.connect('${w.near}/t/acme');
    try {
      // A write goes home, unmarked -- marked, it would be refused -- and
      // says where it left the tenant.
      expect(await db.from('notes').insert({'body': 'a'}), 1);
      final a = db.seq!;
      expect(sent.last['fenec-read'], isNull);
      expect(sent.last['fenec-after'], isNull);
      // The builder's read after it waits for it on the replica: that
      // change of the history the write named, which a node that replicates
      // gives each tenant.
      expect([for (final r in await db.from('notes').rows()) r['body']], ['a']);
      expect(sent.last['fenec-read'], 'replica');
      expect(sent.last['fenec-after'], '$a');
      final history = sent.last['fenec-history'];
      expect(history, matches(RegExp(r'^\d+-[0-9a-f]{16}$')));

      // The copies share the position: a keyed copy's write is what the
      // connection's next read is sent after, and an `asRead()` copy's
      // reads go after the connection's writes.
      expect(await db.withIdempotencyKey('note-b').from('notes').insert({'body': 'b'}), 1);
      final b = db.seq!;
      expect(b, greaterThan(a));
      expect(sent.last['fenec-read'], isNull);
      expect(sent.last['idempotency-key'], 'note-b');
      expect(await db.from('notes').count(), 2);
      expect(sent.last['fenec-read'], 'replica');
      expect(sent.last['fenec-after'], '$b');
      final reader = db.asRead();
      final both = await reader.batch([
        db.from('notes').where('body', 'a').toFenecQL(),
        db.from('notes').where('body', 'b').toFenecQL(),
      ]);
      expect([for (final r in both.results) r.rows.length], [1, 1]);
      expect(sent.last['fenec-read'], 'replica');
      expect(sent.last['fenec-after'], '$b');
      expect((await reader.query('get notes count')).first['count'], 2);
      expect(sent.last['fenec-read'], 'replica');
      expect(sent.last['fenec-history'], history);
      // A text not marked goes home.
      await db.query('get notes count');
      expect(sent.last['fenec-read'], isNull);
      expect(sent.last['fenec-after'], isNull);
      // A write marked by mistake is refused, never landed.
      expect(await status(() => reader.execute('put notes {body: "x"}')), 403);

      // Home gone: the reads near are still answered -- by the replica, the
      // one copy left -- and the connection reading home is not. The
      // replica asked itself first, until it holds both: a read near above
      // may have been answered home, the replica not caught up yet.
      await eventually('the read replica holds both writes', () async => await w.replicaCount() == 2);
      await w.eu1.kill();
      expect([for (final r in await db.from('notes').order('body').rows()) r['body']], ['a', 'b']);
      expect(sent.last['fenec-read'], 'replica');
      expect(sent.last['fenec-after'], '$b');
      expect(await status(() => home.from('notes').rows()), 502);
      expect(await status(() => db.from('notes').insert({'body': 'c'})), 502);
    } finally {
      db.close();
      home.close();
      await sent.close();
      await w.stop();
    }
  });

  // A connection reading home marks nothing, `asRead()` or not.
  test('a connection reads home unless it asks for nearest', () async {
    final sent = await Wire.start(script: []);
    final db = Fenec.connect(sent.url);
    try {
      await db.from('t').rows();
      await db.asRead().query('get t count');
      expect(sent.heads.length, 2);
      for (final h in sent.heads) {
        expect(h['fenec-read'], isNull);
        expect(h['fenec-after'], isNull);
      }
      expect(db.reads, Reads.home);
    } finally {
      db.close();
      await sent.close();
    }
  });

  // Reads near go after the furthest position an answer from home named,
  // through any copy -- not the last answered, and not a read near's own
  // answer, which may be a replica's. A read no copy holds that for (504)
  // is asked home with neither, and home's position, which the 504 names,
  // is the connection's from then.
  test('reads near are sent after the furthest position from home', () async {
    final sent = await Wire.start(script: [
      written('9', h1),
      written('12', h1),
      written('10', h1),
      const Scripted(body: '[{"n":1}]'),
      const Scripted(body: '[{"count":2}]'),
      const Scripted(body: '[{"count":2}]'),
      const Scripted(body: '{"results":[]}', seq: '15', history: h1),
      const Scripted(
          status: 504, body: '{"error":"change 12 is not in this copy\'s history","seq":10,"history":"$h2"}'),
      const Scripted(body: '[{"n":1}]'),
      const Scripted(body: '[{"n":1}]'),
    ]);
    final db = Fenec.connect(sent.url, reads: Reads.nearest);
    try {
      await db.withIdempotencyKey('k1').execute('put t {n: 1}');
      await db.withWait(const Duration(seconds: 1)).execute('set t {n: 2} where n = 1');
      await db.execute('put t {n: 3}');
      await db.from('t').rows();
      expect(await db.from('t').count(), 2);
      await db.query('get t count');
      await db.asRead().batch([db.from('t').toFenecQL()]);
      // The furthest of the three writes, and not the read near's 15.
      expect(db.seq, 12);
      expect((await db.withWait(const Duration(seconds: 1)).from('t').rows()).length, 1);
      await db.from('t').rows();

      final seen = sent.heads;
      expect(seen.length, 10);
      for (final h in seen.take(3)) {
        expect(h['fenec-read'], isNull);
        expect(h['fenec-after'], isNull);
      }
      // The furthest position, 12, not the last answered, 10.
      for (final i in [3, 4, 6, 7]) {
        expect(seen[i]['fenec-read'], 'replica', reason: 'request $i');
        expect(seen[i]['fenec-after'], '12', reason: 'request $i');
        expect(seen[i]['fenec-history'], h1, reason: 'request $i');
      }
      for (final i in [5, 8]) {
        expect(seen[i]['fenec-read'], isNull, reason: 'request $i');
        expect(seen[i]['fenec-after'], isNull, reason: 'request $i');
        expect(seen[i]['fenec-history'], isNull, reason: 'request $i');
      }
      // Home's position taken, lower and of the history after the failover.
      expect(seen[9]['fenec-after'], '10');
      expect(seen[9]['fenec-history'], h2);
      expect(db.seq, 10);
    } finally {
      db.close();
      await sent.close();
    }
  });

  // `seq` is the furthest position home named, as Go's `Seq()` and .NET's
  // `Seq` are: a write answered late does not move it back, and a read
  // near's 504 sets it to where home says it stands. A position of change 0
  // marks a read near and names no change to wait for.
  test('seq is the furthest position home named', () async {
    const h3 = '3-00000000000000cc';
    final held = Completer<void>();
    final sent = await Wire.start(script: [
      const Scripted(body: '{"results":[]}', seq: '0', history: h1),
      const Scripted(),
      Scripted(body: '{"affected":1}', seq: '101', history: h2, gate: held.future),
      written('102', h2),
      const Scripted(),
      refused(7, h3),
      const Scripted(),
      const Scripted(),
    ]);
    final db = Fenec.connect(sent.url, reads: Reads.nearest);
    try {
      await db.batch([db.from('t').toFenecQL()]);
      expect(db.seq, 0);
      await db.from('t').rows();
      expect(sent.last['fenec-read'], 'replica');
      expect(sent.last['fenec-after'], isNull);
      expect(sent.last['fenec-history'], isNull);

      // Two writes, the first answered after the second.
      final first = db.execute('put t {n: 1}');
      await eventually('the first write sent', () => sent.heads.length == 3);
      await db.execute('put t {n: 2}');
      expect(db.seq, 102);
      held.complete();
      await first;
      expect(db.seq, 102);
      await db.from('t').rows();
      expect(sent.last['fenec-after'], '102');

      // A 504 naming a later term's 7: home's word, lower or not.
      await db.from('t').rows();
      expect(db.seq, 7);
      await db.from('t').rows();
      expect(sent.last['fenec-after'], '7');
      expect(sent.last['fenec-history'], h3);
    } finally {
      db.close();
      await sent.close();
    }
  });

  // A read near's 504 sets the position back only over the one the read was
  // sent after -- a write answered meanwhile is further, and set back the
  // next read near would miss it -- and never to an earlier term, which a
  // fenced old primary names.
  test("a read near's 504 sets the position back over its own alone, never to an earlier term", () async {
    final sent = await Wire.start(script: [written('100', h2)]);
    final db = Fenec.connect(sent.url, reads: Reads.nearest);
    try {
      await db.execute('put t {n: 1}');
      // A read near is sent after 100; a write lands at 101 while it waits;
      // then home answers the read 504 at 100.
      final held = Completer<void>();
      sent.script.add(refused(100, h2, gate: held.future));
      final read = db.from('t').rows();
      await eventually('the read sent', () => sent.heads.length == 2);
      expect(sent.last['fenec-after'], '100');
      sent.script.add(written('101', h2));
      await db.execute('put t {n: 2}');
      held.complete();
      await read;
      await db.from('t').rows();
      expect(sent.last['fenec-after'], '101');

      // A 504 naming an earlier term -- the old primary, fenced, reached
      // through a router behind a failover -- is not taken.
      sent.script.add(refused(140, h1));
      await db.from('t').rows();
      await db.from('t').rows();
      expect(sent.last['fenec-after'], '101');
      expect(sent.last['fenec-history'], h2);
    } finally {
      db.close();
      await sent.close();
    }
  });

  // A tenant made again under its name starts again at term 1, of another
  // lineage -- the top half of its history's id: home's position is taken,
  // from a 504 and from an answer, its term lower. Refused as an earlier
  // term, every read near waited its whole wait for good. Within one
  // lineage an earlier term is still refused.
  test('a tenant made again has its position taken, its term lower', () async {
    const old = '3-aaaaaaaa000000c3', remade = '1-bbbbbbbb00000001';
    const third = '1-cccccccc00000001', second = '2-cccccccc00000002';
    final sent = await Wire.start(script: [
      written('50', old),
      refused(2, remade),
      const Scripted(),
      const Scripted(),
      written('1', third),
      const Scripted(),
      written('9', second),
      written('90', third),
      const Scripted(),
    ]);
    final db = Fenec.connect(sent.url, reads: Reads.nearest);
    try {
      await db.execute('put t {n: 1}');
      await db.from('t').rows();
      await db.from('t').rows();
      expect(sent.last['fenec-after'], '2');
      expect(sent.last['fenec-history'], remade);
      await db.execute('put t {n: 2}');
      await db.from('t').rows();
      expect(sent.last['fenec-after'], '1');
      expect(sent.last['fenec-history'], third);
      await db.execute('put t {n: 3}');
      await db.execute('put t {n: 4}');
      await db.from('t').rows();
      expect(sent.last['fenec-after'], '9');
      expect(sent.last['fenec-history'], second);
    } finally {
      db.close();
      await sent.close();
    }
  });
}
