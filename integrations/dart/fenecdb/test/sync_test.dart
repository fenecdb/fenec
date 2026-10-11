// A replica against a real fenec-server the tests start, kill and start
// again on its port: the binary `cargo build -p fenec-server` makes
// (FENEC_SERVER names another), as `make dart-test` builds it.
import 'dart:async';
import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

import 'package:fenecdb/fenecdb.dart';
import 'package:test/test.dart';

class Server {
  final String file;
  Process? _process;
  int port = 0;

  Server._(this.file);

  String get url => 'http://127.0.0.1:$port';

  /// A server over a file of its own, holding `tasks`.
  static Future<Server> start(String name) async {
    final dir = await Directory.systemTemp.createTemp('fenecdb-dart-$name');
    final s = Server._('${dir.path}/server.fenec');
    await s._start(0);
    await s.run('create collection tasks (key text @unique, title text, status text @hash, priority int)');
    await s.run('put tasks [{key: "a", title: "one", status: "open", priority: 1}, '
        '{key: "b", title: "two", status: "open", priority: 5}, '
        '{key: "c", title: "three", status: "closed", priority: 3}]');
    return s;
  }

  Future<void> _start(int at) async {
    final binary =
        Platform.environment['FENEC_SERVER'] ?? '${Directory.current.path}/../../../target/debug/fenec-server';
    final p = await Process.start(binary, ['--http', '127.0.0.1:$at', '--file', file, '--sync', 'always']);
    _process = p;
    p.stdout.drain<void>();
    final found = Completer<int>();
    final seen = StringBuffer();
    p.stderr.transform(utf8.decoder).transform(const LineSplitter()).listen((line) {
      seen.writeln(line);
      final m = RegExp(r'listening on: http://127\.0\.0\.1:(\d+) ').firstMatch(line);
      if (m != null && !found.isCompleted) found.complete(int.parse(m.group(1)!));
    }, onDone: () {
      if (!found.isCompleted) found.completeError(StateError('fenec-server did not start: $seen'));
    });
    port = await found.future.timeout(const Duration(seconds: 20));
  }

  /// Killed, as a crash would: no checkpoint, no goodbye.
  Future<void> kill() async {
    final p = _process;
    if (p == null) return;
    p.kill(ProcessSignal.sigkill);
    await p.exitCode;
    _process = null;
  }

  /// Started again over its file, on the port it had.
  Future<void> restart() => _start(port);

  Future<Answer> run(String text) async {
    final r = Fenec.connect(url);
    try {
      return await r.run(text);
    } finally {
      r.close();
    }
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

Future<String> scratch(String name) async =>
    '${(await Directory.systemTemp.createTemp('fenecdb-dart-$name')).path}/app.fenec';

const open = Shape('tasks', where: {'status': 'open'}, key: 'key');

Future<List<Object?>> titles(Fenec db) async => [
      for (final r in await db.from('tasks').select(['title']).order('title').rows()) r['title']
    ];

void main() {
  final servers = <Server>[];
  Future<Server> server(String name) async => (await Server.start(name))..let(servers.add);
  tearDown(() async {
    for (final s in servers) {
      await s.kill();
    }
    servers.clear();
  });

  test('the seed fills the replica with the shape', () async {
    final s = await server('seed');
    final db = await Fenec.openSynced(url: s.url, shapes: [open], path: await scratch('seed'));
    await db.replica!.ready();
    expect(await titles(db), ['one', 'two']);
    expect((await db.replica!.refresh()).state, SyncState.online);
    // Reads are the file's: the server gone, they go on.
    await s.kill();
    expect(await db.from('tasks').count(), 2);
    await db.close();
  });

  test('a live query runs again on a server write', () async {
    final s = await server('live');
    final db = await Fenec.openSynced(url: s.url, shapes: [open], path: await scratch('live'));
    await db.replica!.ready();
    final seen = <List<Object?>>[];
    final sub = db
        .live(db.from('tasks').select(['title']).order('title'))
        .listen((rows) => seen.add([for (final r in rows) r['title']]));
    await eventually('the first rows', () => seen.isNotEmpty);
    await s.run('put tasks {key: "d", title: "four", status: "open"}');
    await eventually("the server's write", () => seen.isNotEmpty && seen.last.join(',') == 'four,one,two');
    await sub.cancel();
    await db.close();
  });

  test('an optimistic write shows at once and lands', () async {
    final s = await server('optimistic');
    final db = await Fenec.openSynced(url: s.url, shapes: [open], path: await scratch('optimistic'));
    await db.replica!.ready();
    // Offline, nothing is sent: the server's answer cannot replace the row
    // before it is read (see 'a refused write is put back').
    db.replica!.setOnline(false);
    await db.from('tasks').insert({'title': 'new', 'status': 'open', 'priority': 2});
    // At once, under a temporary id.
    expect((await db.from('tasks').where('title', 'new').first())!['id'] as int, greaterThanOrEqualTo(1 << 52));
    db.replica!.setOnline(true);
    await db.replica!.pushed();
    await eventually("the server's copy in place of the temporary row", () async {
      final rows = await db.from('tasks').where('title', 'new').rows();
      return rows.length == 1 && (rows[0]['id'] as int) < 1 << 52;
    });
    expect((await s.run('get tasks where title = "new"')).rows, hasLength(1));
    // An update and a delete go the same way.
    await db.from('tasks').where('key', 'a').update({'title': 'ONE'});
    await db.from('tasks').where('key', 'b').delete();
    await db.replica!.pushed();
    expect((await s.run('get tasks where title = "ONE"')).rows, hasLength(1));
    expect((await s.run('get tasks where key = "b"')).rows, isEmpty);
    await db.close();
  });

  test('a refused write is put back', () async {
    final s = await server('refused');
    final db = await Fenec.openSynced(url: s.url, shapes: [open], path: await scratch('refused'));
    await db.replica!.ready();
    final refused = <Refusal>[];
    final sub = db.replica!.refusals.listen(refused.add);
    // Offline, nothing is sent, so the row is read before the server can
    // refuse it: online, the 409 could come back and the row be put back
    // before the read, as it was in the Kotlin test on a loaded runner.
    db.replica!.setOnline(false);
    // The server's key is @unique; the replica's a plain hash.
    await db.from('tasks').insert({'key': 'a', 'title': 'dup', 'status': 'open'});
    expect(await titles(db), ['dup', 'one', 'two']);
    db.replica!.setOnline(true);
    await eventually('the refusal', () => refused.isNotEmpty);
    expect(refused.first.status, 409);
    await eventually('the write put back', () async => (await titles(db)).join(',') == 'one,two');
    expect((await db.replica!.refresh()).error?.status, 409);
    expect((await db.replica!.refresh()).pending, 0);
    await sub.cancel();
    await db.close();
  });

  test('a server killed and started again is caught up with', () async {
    final s = await server('restart');
    final db = await Fenec.openSynced(url: s.url, shapes: [open], path: await scratch('restart'));
    await db.replica!.ready();
    await s.kill();
    await eventually('offline', () => db.replica!.status.state == SyncState.offline);
    // A write while it is down waits in the file.
    await db.from('tasks').insert({'title': 'while down', 'status': 'open'});
    expect((await db.replica!.refresh()).pending, 1);
    await s.restart();
    await s.run('put tasks {key: "e", title: "after", status: "open"}');
    db.replica!.resume();
    await eventually('caught up both ways', () async {
      return (await titles(db)).join(',') == 'after,one,two,while down' && (await db.replica!.refresh()).pending == 0;
    });
    expect((await s.run('get tasks where title = "while down"')).rows, hasLength(1));
    await db.close();
  });

  test('pending writes outlive a reopen', () async {
    final s = await server('reopen');
    final path = await scratch('reopen');
    var db = await Fenec.openSynced(url: s.url, shapes: [open], path: path);
    await db.replica!.ready();
    await s.kill();
    await db.from('tasks').insert({'title': 'kept', 'status': 'open'});
    await db.from('tasks').where('key', 'a').update({'title': 'ONE'});
    expect((await db.replica!.refresh()).pending, 2);
    await db.close();

    db = await Fenec.openSynced(url: s.url, shapes: [open], path: path);
    expect((await db.replica!.refresh()).pending, 2);
    expect(await titles(db), ['ONE', 'kept', 'two']);
    await s.restart();
    db.replica!.resume();
    await eventually('the queue sent', () async => (await db.replica!.refresh()).pending == 0);
    expect((await s.run('get tasks where title = "kept"')).rows, hasLength(1));
    expect((await s.run('get tasks where title = "ONE"')).rows, hasLength(1));
    await eventually(
        "the server's copies", () async => (await db.from('tasks').rows()).every((r) => (r['id'] as int) < 1 << 52));
    await db.close();
  });

  // The Dart tab's "over HTTP" lines on languages.html, as written there
  // but for the server's address.
  test("the docs' example over HTTP", () async {
    final s = await server('docs');
    await s.run('create collection if not exists docs (title text, embed vector<3> @hnsw(cosine))');
    await s.run('put docs {title: "Night at the oasis", embed: [0.1, 0.2, 0.3]}');

    final remote = Fenec.connect(s.url, token: 'secret');
    await remote.execute(r'put docs {title: $1, embed: $2}', [
      'Dunes',
      Float32List.fromList([0.9, 0.1, 0.0])
    ]);
    final hits = await remote
        .from('docs')
        .select(['title'])
        .near('embed', Float32List.fromList([0.1, 0.2, 0.3]))
        .limit(5)
        .rows();
    expect([for (final r in hits) r['title']], ['Night at the oasis', 'Dunes']);
    remote.close();
  });

  test('connect runs every query on the server', () async {
    final s = await server('connect');
    final db = Fenec.connect(s.url);
    final rows = await db.from('tasks').where('status', 'open').order('priority', 'desc').rows();
    expect([for (final r in rows) r['title']], ['two', 'one']);
    expect(await db.from('tasks').insert({'key': 'z', 'title': 'remote'}), 1);
    expect(await db.from('tasks').count(), 4);
    await expectLater(
      db.from('tasks').insert({'key': 'z', 'title': 'again'}),
      throwsA(isA<FenecException>().having((e) => e.code, 'code', FenecCode.duplicate)),
    );
    db.close();
  });

  test('a batch lands whole, says where it stopped, and lands once under a key', () async {
    final s = await server('batch');
    final db = Fenec.connect(s.url);
    final tasks = db.from('tasks');
    final out = await db.batch([
      tasks.toInsert({'key': 'd', 'title': 'four'}),
      tasks.where('key', 'a').toUpdate({'priority': 9}, require: 1),
      tasks.where('priority', '>=', 5).order('priority').select(['key']).toFenecQL(),
    ]);
    expect(out.results, hasLength(3));
    expect(out.results[0].affected, 1);
    expect([for (final r in out.results[2].rows) r['key']], ['b', 'a']);
    expect(out.seq, isNotNull);
    expect(out.seq, db.seq);
    expect(out.replayed, isFalse);

    await expectLater(
      db.batch([
        tasks.toInsert({'key': 'e', 'title': 'five'}),
        tasks.where('key', 'nobody').toDelete(require: 1),
      ]),
      throwsA(isA<FenecException>()
          .having((e) => e.code, 'code', FenecCode.unmet)
          .having((e) => e.status, 'status', 412)
          .having((e) => e.at, 'at', 1)
          .having((e) => e.completed, 'completed', 0)),
    );
    expect(await tasks.count(), 4);

    final stmts = [
      tasks.toInsert({'key': 'f', 'title': 'six'})
    ];
    expect((await db.batch(stmts, idempotencyKey: 'batch-1')).replayed, isFalse);
    expect((await db.batch(stmts, idempotencyKey: 'batch-1')).replayed, isTrue);
    expect(await tasks.count(), 5);
    // A copy keys every write it runs, the builder's too.
    final keyed = db.withIdempotencyKey('put-1');
    expect(await keyed.from('tasks').insert({'key': 'g'}), 1);
    expect(await keyed.from('tasks').insert({'key': 'g'}), 1);
    expect(await tasks.count(), 6);
    await expectLater(
      keyed.execute('put tasks {key: "i"}'),
      throwsA(isA<FenecException>().having((e) => e.status, 'status', 422).having((e) => e.at, 'at', isNull)),
    );
    db.close();
  });

  test('a claim held at the server takes the job enqueued meanwhile, and ends on time', () async {
    final s = await server('held');
    await s.run('create collection jobs (kind text, run_at timestamp @sorted, owner text)');
    final db = Fenec.connect(s.url);
    Future<Rows> claim(FenecRemote c, String owner) =>
        c.from('jobs').where(Cond.raw('run_at <= now()')).order('run_at').limit(1).updateReturning({
          'owner': owner,
          'run_at': Computed.expr('now() + ?', [60000])
        }, returning: [
          'kind'
        ]);
    final held = claim(db.withWait(const Duration(seconds: 10)), 'w1');
    await Future<void>.delayed(const Duration(milliseconds: 200));
    await db.execute('put jobs {kind: "mail", run_at: \$1}', [DateTime.now().millisecondsSinceEpoch]);
    expect([for (final r in await held.timeout(const Duration(seconds: 5))) r['kind']], ['mail']);
    final began = DateTime.now();
    expect(await claim(db.withWait(const Duration(milliseconds: 300)), 'w2'), isEmpty);
    expect(DateTime.now().difference(began).inMilliseconds, greaterThanOrEqualTo(290));
    db.close();
  });

  test('a server answers marks in the row and facets beside the rows', () async {
    final s = await server('search');
    await s.run('create collection docs (body text @text, kind text)');
    await s.run('put docs [{body: "rust is fast", kind: "a"}, {body: "rust and go", kind: "a"}, '
        '{body: "python", kind: "b"}]');
    final db = Fenec.connect(s.url);
    final hits = await db.from('docs').select(['kind']).highlight('body').match('body', 'rust').rows();
    expect([
      for (final r in hits) r['highlight(body)']
    ], [
      [
        [0, 4]
      ],
      [
        [0, 4]
      ]
    ]);
    expect(hits.facets, isNull);
    // `/query` answers an object here: the rows, and the counts of every
    // matching row beside them.
    final a = await db.from('docs').facet('kind').limit(1).answer();
    expect(a.rows, hasLength(1));
    expect(a.facets, {
      'kind': [const FacetCount('a', 2), const FacetCount('b', 1)]
    });
    final rows = await db.from('docs').where('kind', 'a').facet('kind', top: 1).rows();
    expect(rows, hasLength(2));
    expect(rows.facets, {
      'kind': [const FacetCount('a', 2)]
    });
    db.close();
  });
}

extension<T> on T {
  void let(void Function(T) f) => f(this);
}
