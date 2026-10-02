part of 'fenec.dart';

/// The rows of one collection a replica keeps: a filter the server applies,
/// in the JS sync layer's form -- `{'status': 'open', 'priority': {'gte': 3}}`
/// -- the fields it carries, and the business key an optimistic insert is
/// matched with the server's copy by (a `text @hash` field).
class Shape {
  final String collection;
  final Map<String, Object?>? where;
  final List<String>? select;
  final String? key;

  const Shape(this.collection, {this.where, this.select, this.key});

  Map<String, Object?> _json() => {
        'collection': collection,
        if (where != null) 'where': normalize(where),
        if (select != null) 'select': select,
        if (key != null) 'key': key,
      };
}

/// Where a replica stands with its server.
enum SyncState {
  /// Every shape's stream is open and seeded.
  online,

  /// The server cannot be reached, or the app said the network is gone.
  offline,

  /// Connecting, seeding, or sending the writes left from before.
  catchingUp,
}

/// An error, and its HTTP status (0: the network, or the replica).
class SyncFailure {
  final String message;
  final int status;
  const SyncFailure(this.message, this.status);
  @override
  String toString() => 'SyncFailure($status): $message';
}

class ShapeState {
  final String collection;
  final int cursor;
  final bool seeded;
  final bool connected;
  const ShapeState(this.collection, this.cursor, this.seeded, this.connected);
}

class SyncStatus {
  final SyncState state;

  /// Writes the server has not answered.
  final int pending;

  /// The last error: a connection's until it is back, a refused write's
  /// until the next.
  final SyncFailure? error;
  final List<ShapeState> shapes;

  const SyncStatus(this.state, this.pending, this.error, this.shapes);

  /// Whether every shape has had its seed.
  bool get seeded => shapes.isNotEmpty && shapes.every((s) => s.seeded);

  static const _starting = SyncStatus(SyncState.catchingUp, 0, null, []);

  static SyncStatus _of(Map<String, Object?> v) {
    final e = v['error'] as Map<String, Object?>?;
    return SyncStatus(
      switch (v['state']) {
        'online' => SyncState.online,
        'offline' => SyncState.offline,
        _ => SyncState.catchingUp,
      },
      v['pending'] as int? ?? 0,
      e == null ? null : SyncFailure(e['message'] as String? ?? '', e['status'] as int? ?? 0),
      [
        for (final s in (v['shapes'] as List? ?? const []).cast<Map<String, Object?>>())
          ShapeState(s['collection'] as String, s['cursor'] as int, s['seeded'] == true, s['connected'] == true),
      ],
    );
  }
}

/// A write the server refused: it was put back on the replica.
class Refusal {
  final int status;
  final String message;

  /// The statement's text, as it was run.
  final String query;
  const Refusal(this.status, this.message, this.query);
}

const _kPoll = 0, _kResponse = 1, _kOpened = 2, _kBytes = 3, _kClosed = 4, _kTimer = 5, _kSignal = 6;

/// A replica's sync with its server: its status, its token, and the network
/// as the app sees it. The engine is told of what happens one thing at a
/// time, in order (a chain of futures), so a stream's pieces arrive as they
/// came and an action is never performed before the one it cancels.
class Replica {
  final Fenec _db;
  final String _url;
  String? _token;
  final Future<String> Function()? _provider;
  final HttpClient _client = HttpClient()..connectionTimeout = const Duration(seconds: 10);
  final _requests = <int, HttpClientRequest>{};
  final _streams = <int, StreamSubscription<List<int>>>{};
  final _statuses = StreamController<SyncStatus>.broadcast();
  final _refusals = StreamController<Refusal>.broadcast();
  var _status = SyncStatus._starting;
  Future<void> _chain = Future.value();
  var _stopped = false;

  Replica._(this._db, String url, this._token, this._provider)
      : _url = url.endsWith('/') ? url.substring(0, url.length - 1) : url;

  /// The status the sync last gave, which never waits.
  SyncStatus get status => _status;

  /// The status at every change.
  Stream<SyncStatus> get statuses => _statuses.stream;

  /// The writes the server refused, each put back on the replica.
  Stream<Refusal> get refusals => _refusals.stream;

  /// The status once everything told the sync before this call is in it --
  /// a write just made among its pending ones.
  Future<SyncStatus> refresh() => _serial(() async {
        await _reload();
        return _status;
      });

  /// Completes once every shape has had its seed: at once for a replica
  /// opened again, which resumes where it stopped.
  Future<void> ready() => _until((s) => s.seeded);

  /// Completes once the server has answered every write made so far.
  Future<void> pushed() => _until((s) => s.pending == 0);

  Future<void> _until(bool Function(SyncStatus) done) async {
    final wait = statuses.firstWhere(done);
    if (done(await refresh())) return;
    await wait;
  }

  /// A fresh token, for the requests and streams from here on.
  void setToken(String token) {
    _token = token;
    _signalJson({'token': token});
  }

  /// The network as the platform sees it (connectivity_plus, say): offline
  /// closes the streams and sends nothing; online comes back at once, the
  /// backoff forgotten.
  void setOnline(bool online) => _signalJson({'online': online});

  /// What an app calls as it is resumed (`AppLifecycleState.resumed`): the
  /// streams a pause cut are opened again at once, from their cursors.
  void resume() => setOnline(true);

  /// The server itself, for what the replica does not hold.
  FenecRemote get remote => FenecRemote(_url, token: _token);

  // ---------------------------------------------------------- the engine

  Future<T> _serial<T>(Future<T> Function() f) {
    final next = _chain.then((_) => f());
    _chain = next.then((_) {}, onError: (_) {});
    return next;
  }

  void _poll() {
    if (!_stopped) _serial(() => _feed(_kPoll, 0));
  }

  Future<void> _stop() async {
    if (_stopped) return;
    await _serial(() => _feed(_kSignal, 0, bytes: utf8.encode('{"stop":true}')));
    _stopped = true;
    for (final s in _streams.values) {
      await s.cancel();
    }
    for (final r in _requests.values) {
      r.abort();
    }
    _streams.clear();
    _requests.clear();
    _client.close(force: true);
    await _statuses.close();
    await _refusals.close();
  }

  void _signalJson(Map<String, Object?> fields) {
    if (!_stopped) _serial(() => _feed(_kSignal, 0, bytes: utf8.encode(jsonEncode(fields))));
  }

  Future<void> _feed(int kind, int id, {int status = 0, int seq = 0, List<int>? bytes}) async {
    if (_stopped) return;
    final (code, text) = await _db._worker.call('syncFeed', [
      _db._handle,
      kind,
      id,
      status,
      seq,
      bytes == null ? null : (bytes is Uint8List ? bytes : Uint8List.fromList(bytes)),
    ]);
    if (code == 0) await _perform(text);
  }

  Future<void> _perform(String json) async {
    final actions = (jsonDecode(json) as List).cast<Map<String, Object?>>();
    for (final a in actions) {
      final id = a['id'] as int? ?? 0;
      switch (a['do']) {
        case 'request':
          _request(id, a);
        case 'stream':
          _stream(id, a);
        case 'cancel':
          _streams.remove(id)?.cancel();
          _requests.remove(id)?.abort();
        case 'wait':
          Timer(Duration(milliseconds: a['ms'] as int), () {
            if (!_stopped) _serial(() => _feed(_kTimer, id));
          });
        case 'token':
          final p = _provider;
          if (p != null) {
            p().then(setToken, onError: (_) {});
          }
        case 'changed':
          _db.lives.touch();
        case 'refused':
          if (!_refusals.isClosed) {
            _refusals.add(Refusal(a['status'] as int, a['message'] as String, a['query'] as String));
          }
        case 'status':
          await _reload();
      }
    }
  }

  Future<void> _reload() async {
    final (code, text) = await _db._worker.call('syncStatus', [_db._handle]);
    if (code != 0) return;
    _status = SyncStatus._of(jsonDecode(text) as Map<String, Object?>);
    if (!_statuses.isClosed) _statuses.add(_status);
  }

  Future<HttpClientRequest> _open(Map<String, Object?> a) async {
    final req = await _client.openUrl(a['method'] as String? ?? 'GET', Uri.parse(a['url'] as String));
    (a['headers'] as Map<String, Object?>? ?? const {}).forEach((k, v) => req.headers.set(k, v as String));
    final body = a['body'] as String?;
    if (body != null) {
      // fenec-server reads a body by its length: a chunked one is refused.
      final bytes = utf8.encode(body);
      req.contentLength = bytes.length;
      req.add(bytes);
    }
    return req;
  }

  /// A request, its answer handed on in turn.
  void _request(int id, Map<String, Object?> a) {
    () async {
      var status = 0, seq = 0;
      List<int> body;
      try {
        final req = await _open(a);
        _requests[id] = req;
        final res = await req.close().timeout(const Duration(seconds: 30));
        status = res.statusCode;
        seq = int.tryParse(res.headers.value('fenec-seq') ?? '') ?? 0;
        body = await res.fold<List<int>>(<int>[], (all, piece) => all..addAll(piece));
      } catch (e) {
        status = 0;
        body = utf8.encode('$e');
      }
      _requests.remove(id);
      if (!_stopped) _serial(() => _feed(_kResponse, id, status: status, seq: seq, bytes: body));
    }();
  }

  /// A stream, each piece handed on in turn as it comes.
  void _stream(int id, Map<String, Object?> a) {
    () async {
      try {
        final req = await _open(a);
        _requests[id] = req;
        final res = await req.close();
        _requests.remove(id);
        if (res.statusCode != 200) {
          final body = await res.fold<List<int>>(<int>[], (all, piece) => all..addAll(piece));
          _serial(() => _feed(_kOpened, id, status: res.statusCode, bytes: body));
          return;
        }
        _serial(() => _feed(_kOpened, id, status: 200));
        // The server's keepalive comes every 20 s: a silence past it is a
        // connection gone.
        _streams[id] = res.timeout(const Duration(seconds: 90)).listen(
          (piece) => _serial(() => _feed(_kBytes, id, bytes: piece)),
          onError: (Object e) {
            _streams.remove(id)?.cancel();
            _serial(() => _feed(_kClosed, id, bytes: utf8.encode('$e')));
          },
          onDone: () {
            _streams.remove(id);
            _serial(() => _feed(_kClosed, id));
          },
          cancelOnError: true,
        );
      } catch (e) {
        _requests.remove(id);
        if (!_stopped) _serial(() => _feed(_kClosed, id, bytes: utf8.encode('$e')));
      }
    }();
  }
}

/// Opens [path] as a replica of [shapes], kept in step with the server at
/// [url] -- see [Fenec.openSynced].
Future<Fenec> _openSynced({
  required String url,
  String? token,
  required List<Shape> shapes,
  required String path,
  int flags = 0,
  Future<String> Function()? tokenProvider,
}) async {
  final db = await Fenec.open(path, flags: flags);
  try {
    final r = Random.secure();
    final seed = List.generate(32, (_) => r.nextInt(16).toRadixString(16)).join();
    final config = jsonEncode({
      'url': url,
      if (token != null) 'token': token,
      'seed': seed,
      'shapes': [for (final s in shapes) s._json()],
    });
    final first = Fenec._answer(await db._worker.call('syncStart', [db._handle, config]));
    final replica = Replica._(db, url, token, tokenProvider);
    db._replica = replica;
    replica._serial(() => replica._perform(first));
    return db;
  } catch (_) {
    await db.close();
    rethrow;
  }
}
