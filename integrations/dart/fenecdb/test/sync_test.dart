// A replica against a real fenec-server the tests start, kill and start
// again on its port: the binary `cargo build -p fenec-server` makes
// (FENEC_SERVER names another), as `make dart-test` builds it.
import 'dart:async';
import 'dart:convert';
import 'dart:io';

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
    await db.from('tasks').insert({'title': 'new', 'status': 'open', 'priority': 2});
    // At once, under a temporary id.
    expect((await db.from('tasks').where('title', 'new').first())!['id'] as int, greaterThanOrEqualTo(1 << 52));
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
    // The server's key is @unique; the replica's a plain hash.
    await db.from('tasks').insert({'key': 'a', 'title': 'dup', 'status': 'open'});
    expect(await titles(db), ['dup', 'one', 'two']);
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
}

extension<T> on T {
  void let(void Function(T) f) => f(this);
}
