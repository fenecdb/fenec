import 'dart:async';

import 'fenec.dart';
import 'query.dart';

class _Sub {
  final Future<List<Map<String, Object?>>> Function() rows;
  final Set<String>? reads;
  final StreamController<List<Map<String, Object?>>> out;
  var on = true;
  _Sub(this.rows, this.reads, this.out);
}

/// The live queries over one database, as `web/fenec.js`'s `Lives` keeps a
/// page's: the change ring says which collections were written since a
/// cursor ([Fenec.changes]), a block's once it lands whole, so knowing costs
/// a write nothing -- it is asked once after a burst of writes, and only by
/// a database holding a live query. Collection granularity: a query that
/// reads what was written runs again from scratch, well under a millisecond
/// over a local file, where anything finer would cost more than the query.
///
/// A write through the database asks for a look ([touch]); the looks a
/// burst asks for are one, taken a frame later and once no write is under
/// way, so a loop of writes or several at once run each query once.
class Lives {
  /// How long a burst's looks are gathered: a frame at 60 Hz, the soonest a
  /// screen shows anything.
  static const gather = Duration(milliseconds: 16);

  final Fenec _db;
  final _subs = <_Sub>[];
  var _cursor = 0;
  Timer? _due;

  /// One look at a time: a look awaits the database between its steps.
  Future<void> _looking = Future.value();

  Lives(this._db);

  void touch() {
    if (_subs.isEmpty || _due != null) return;
    _due = Timer(gather, () {
      _due = null;
      // A write under way asks again as it ends.
      if (_db.writing) return;
      _serial(_tick);
    });
  }

  Future<void> _serial(Future<void> Function() f) {
    final next = _looking.then((_) => f()).catchError((_) {});
    _looking = next;
    return next;
  }

  Stream<List<Map<String, Object?>>> stream(String text, List<Object?> params, Set<String>? reads, Object? failure) {
    late final _Sub sub;
    final out = StreamController<List<Map<String, Object?>>>(
      onListen: () => _serial(() async {
        // With none before it no look has kept the cursor up: it starts here,
        // where the first run reads.
        if (_subs.isEmpty) _cursor = (await _db.changes(9007199254740991)).seq;
        _subs.add(sub);
        await _run(sub);
      }),
      onCancel: () {
        sub.on = false;
        _subs.remove(sub);
      },
    );
    sub = _Sub(() async {
      if (failure != null) throw failure;
      return (await _db.runQuiet(text, params, quiet: true)).rows;
    }, reads, out);
    return out.stream;
  }

  void clear() {
    _due?.cancel();
    for (final s in _subs) {
      s.on = false;
      s.out.close();
    }
    _subs.clear();
  }

  Future<void> _tick() async {
    final Changes info;
    try {
      info = await _db.changes(_cursor);
    } catch (_) {
      return;
    }
    _cursor = info.seq;
    final dirty = info.collections?.toSet();
    // Only reads since: nothing to run.
    if (dirty != null && dirty.isEmpty) return;
    for (final s in List.of(_subs)) {
      if (dirty != null && s.reads != null && !s.reads!.any(dirty.contains)) continue;
      await _run(s);
    }
  }

  Future<void> _run(_Sub s) async {
    try {
      final rows = await s.rows();
      // Stopped while it ran: its rows go nowhere.
      if (s.on) s.out.add(rows);
    } catch (e) {
      if (s.on) {
        s.on = false;
        _subs.remove(s);
        s.out.addError(e);
        await s.out.close();
      }
    }
  }
}

extension LiveQueries on Fenec {
  /// The rows of [query] now, and again after every write to a collection it
  /// reads -- the writes of a burst run it once. An error ends the stream;
  /// a `StreamBuilder` takes it as it is.
  Stream<List<Map<String, Object?>>> live(Query query) {
    try {
      final s = query.toFenecQL();
      return lives.stream(s.text, s.params, query.reads?.toSet(), null);
    } on FenecException catch (e) {
      // A chain the builder refuses is the live query's error, where its
      // rows would have been.
      return lives.stream('', const [], null, e);
    }
  }

  /// A live FenecQL text: run again after every write to [collections], or
  /// to anything when it names none.
  Stream<List<Map<String, Object?>>> liveText(String text,
          {List<Object?> params = const [], List<String>? collections}) =>
      lives.stream(text, params, collections?.toSet(), null);
}
