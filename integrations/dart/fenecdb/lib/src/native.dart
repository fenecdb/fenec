// The native library (crates/fenec-ffi) through dart:ffi, and the isolate
// its calls run on.
//
// A call may block -- for the database's lock, an fsync, a whole open -- and
// a Dart FFI call blocks the isolate that makes it: on Flutter's UI isolate
// that is a frame dropped. So every call goes to a worker isolate a
// database keeps for itself, as a message, and its answer comes back as
// one. A handle is a number the library hands out, good on any isolate.

import 'dart:async';
import 'dart:convert';
import 'dart:ffi';
import 'dart:io';
import 'dart:isolate';
import 'dart:typed_data';

import 'package:ffi/ffi.dart';

typedef _OpenC = Int32 Function(Pointer<Uint8>, Size, Uint32, Pointer<Uint64>, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _OpenD = int Function(Pointer<Uint8>, int, int, Pointer<Uint64>, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _MemoryC = Int32 Function(Pointer<Uint64>, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _MemoryD = int Function(Pointer<Uint64>, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _HandleC = Int32 Function(Uint64, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _HandleD = int Function(int, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _QueryC = Int32 Function(
    Uint64, Pointer<Uint8>, Size, Pointer<Uint8>, Size, Pointer<Uint8>, Size, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _QueryD = int Function(
    int, Pointer<Uint8>, int, Pointer<Uint8>, int, Pointer<Uint8>, int, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _ChangesC = Int32 Function(Uint64, Uint64, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _ChangesD = int Function(int, int, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _SyncStartC = Int32 Function(Uint64, Pointer<Uint8>, Size, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _SyncStartD = int Function(int, Pointer<Uint8>, int, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _SyncFeedC = Int32 Function(
    Uint64, Uint32, Uint64, Int32, Uint64, Pointer<Uint8>, Size, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _SyncFeedD = int Function(int, int, int, int, int, Pointer<Uint8>, int, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _SchemaC = Int32 Function(Uint64, Pointer<Uint8>, Size, Uint32, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _SchemaD = int Function(int, Pointer<Uint8>, int, int, Pointer<Pointer<Char>>, Pointer<Size>);
typedef _FreeC = Void Function(Pointer<Char>);
typedef _FreeD = void Function(Pointer<Char>);
typedef _VersionC = Pointer<Char> Function();

/// Where the library is: what `Fenec.library` was set to, else
/// `FENEC_LIBRARY`, else the platform's place -- on iOS and macOS the
/// `FenecFFI.framework` the Flutter plugin's pod embeds in the app, found
/// through the app's `@rpath` as Flutter's FFI plugins open theirs;
/// `libfenec_ffi.so` beside the app on Android and Linux. A macOS program
/// without the plugin falls back to `libfenec_ffi.dylib` on the loader's
/// path.
DynamicLibrary loadLibrary(String? path) {
  path ??= Platform.environment['FENEC_LIBRARY'];
  if (path != null) return DynamicLibrary.open(path);
  if (Platform.isIOS) return DynamicLibrary.open('FenecFFI.framework/FenecFFI');
  if (Platform.isMacOS) {
    try {
      return DynamicLibrary.open('FenecFFI.framework/FenecFFI');
    } on ArgumentError {
      return DynamicLibrary.open('libfenec_ffi.dylib');
    }
  }
  if (Platform.isWindows) return DynamicLibrary.open('fenec_ffi.dll');
  return DynamicLibrary.open('libfenec_ffi.so');
}

/// The library's functions, looked up once an isolate.
class Native {
  final _OpenD _open;
  final _MemoryD _memory;
  final _QueryD _query;
  final _ChangesD _changes;
  final _HandleD _close, _sync, _flush, _checkpoint, _syncStatus;
  final _SyncStartD _syncStart;
  final _SyncFeedD _syncFeed;
  final _SchemaD _schema;
  final _FreeD _free;
  final Pointer<Char> Function() _version;

  Native(DynamicLibrary lib)
      : _open = lib.lookupFunction<_OpenC, _OpenD>('fenec_open'),
        _memory = lib.lookupFunction<_MemoryC, _MemoryD>('fenec_open_memory'),
        _query = lib.lookupFunction<_QueryC, _QueryD>('fenec_query'),
        _changes = lib.lookupFunction<_ChangesC, _ChangesD>('fenec_changes'),
        _close = lib.lookupFunction<_HandleC, _HandleD>('fenec_close'),
        _sync = lib.lookupFunction<_HandleC, _HandleD>('fenec_sync'),
        _flush = lib.lookupFunction<_HandleC, _HandleD>('fenec_flush'),
        _checkpoint = lib.lookupFunction<_HandleC, _HandleD>('fenec_checkpoint'),
        _syncStatus = lib.lookupFunction<_HandleC, _HandleD>('fenec_sync_status'),
        _syncStart = lib.lookupFunction<_SyncStartC, _SyncStartD>('fenec_sync_start'),
        _syncFeed = lib.lookupFunction<_SyncFeedC, _SyncFeedD>('fenec_sync_feed'),
        _schema = lib.lookupFunction<_SchemaC, _SchemaD>('fenec_schema'),
        _free = lib.lookupFunction<_FreeC, _FreeD>('fenec_free_string'),
        _version = lib.lookupFunction<_VersionC, Pointer<Char> Function()>('fenec_version');

  String get version => _version().cast<Utf8>().toDartString();

  /// One call: its code and what it wrote through its out pointer.
  (int, String) _call(int Function(Pointer<Pointer<Char>>, Pointer<Size>) f) {
    final out = calloc<Pointer<Char>>();
    final len = calloc<Size>();
    try {
      final code = f(out, len);
      final p = out.value;
      if (p == nullptr) return (code, '');
      try {
        return (code, utf8.decode(p.cast<Uint8>().asTypedList(len.value)));
      } finally {
        _free(p);
      }
    } finally {
      calloc.free(out);
      calloc.free(len);
    }
  }

  /// Bytes copied where the library reads them; null for none.
  Pointer<Uint8> _bytes(Uint8List b) {
    final p = malloc<Uint8>(b.isEmpty ? 1 : b.length);
    p.asTypedList(b.isEmpty ? 1 : b.length).setRange(0, b.length, b);
    return p;
  }

  (int, String) open(String path, int flags) {
    final b = utf8.encode(path);
    final p = _bytes(b);
    final h = calloc<Uint64>();
    try {
      final (code, text) = _call((out, len) => _open(p, b.length, flags, h, out, len));
      return (code, code == 0 ? '${h.value}' : text);
    } finally {
      malloc.free(p);
      calloc.free(h);
    }
  }

  (int, String) memory() {
    final h = calloc<Uint64>();
    try {
      final (code, text) = _call((out, len) => _memory(h, out, len));
      return (code, code == 0 ? '${h.value}' : text);
    } finally {
      calloc.free(h);
    }
  }

  (int, String) query(int handle, String text, String params, Uint8List? vectors) {
    final t = utf8.encode(text), q = utf8.encode(params);
    final tp = _bytes(t), qp = _bytes(q);
    final vp = vectors == null ? nullptr.cast<Uint8>() : _bytes(vectors);
    try {
      return _call((out, len) => _query(handle, tp, t.length, qp, q.length, vp, vectors?.length ?? 0, out, len));
    } finally {
      malloc.free(tp);
      malloc.free(qp);
      if (vectors != null) malloc.free(vp);
    }
  }

  (int, String) changes(int handle, int since) => _call((out, len) => _changes(handle, since, out, len));

  (int, String) syncStart(int handle, String config) {
    final b = utf8.encode(config);
    final p = _bytes(b);
    try {
      return _call((out, len) => _syncStart(handle, p, b.length, out, len));
    } finally {
      malloc.free(p);
    }
  }

  /// A schema declared as FenecQL against the database (`fenec_schema`):
  /// mode 0 plans, 1 applies.
  (int, String) schema(int handle, String request, int mode) {
    final b = utf8.encode(request);
    final p = _bytes(b);
    try {
      return _call((out, len) => _schema(handle, p, b.length, mode, out, len));
    } finally {
      malloc.free(p);
    }
  }

  (int, String) syncFeed(int handle, int kind, int id, int status, int seq, Uint8List? bytes) {
    final p = bytes == null ? nullptr.cast<Uint8>() : _bytes(bytes);
    try {
      return _call((out, len) => _syncFeed(handle, kind, id, status, seq, p, bytes?.length ?? 0, out, len));
    } finally {
      if (bytes != null) malloc.free(p);
    }
  }

  (int, String) byHandle(String op, int handle) => _call((out, len) => switch (op) {
        'close' => _close(handle, out, len),
        'sync' => _sync(handle, out, len),
        'flush' => _flush(handle, out, len),
        'syncStatus' => _syncStatus(handle, out, len),
        _ => _checkpoint(handle, out, len),
      });
}

/// The isolate a database's calls run on, and the answers they wait for.
class Worker {
  final Isolate _isolate;
  final SendPort _send;
  final ReceivePort _receive;
  final _waiting = <int, Completer<(int, String)>>{};
  var _next = 0;

  Worker._(this._isolate, this._send, this._receive) {
    _receive.listen((m) {
      final [int id, int code, String text] = m as List;
      _waiting.remove(id)?.complete((code, text));
    });
  }

  static Future<Worker> start(String? library) async {
    final first = ReceivePort();
    final isolate = await Isolate.spawn(_main, [first.sendPort, library], debugName: 'fenecdb');
    final port = Completer<SendPort>();
    final answers = ReceivePort();
    first.listen((m) {
      if (m is SendPort) {
        m.send(answers.sendPort);
        port.complete(m);
        first.close();
      } else if (m is List && !port.isCompleted) {
        // The library could not be loaded there.
        port.completeError(StateError(m[0] as String));
        first.close();
      }
    });
    try {
      return Worker._(isolate, await port.future, answers);
    } catch (_) {
      answers.close();
      isolate.kill();
      rethrow;
    }
  }

  /// Runs `op` on the worker: its code and its text.
  Future<(int, String)> call(String op, List<Object?> args) {
    final id = _next++;
    final c = Completer<(int, String)>();
    _waiting[id] = c;
    _send.send([id, op, ...args]);
    return c.future;
  }

  void stop() {
    _receive.close();
    _isolate.kill(priority: Isolate.beforeNextEvent);
  }

  static void _main(List<Object?> start) {
    final reply = start[0] as SendPort;
    final Native native;
    try {
      native = Native(loadLibrary(start[1] as String?));
    } catch (e) {
      reply.send(['fenecdb: the native library could not be loaded: $e']);
      return;
    }
    final requests = ReceivePort();
    reply.send(requests.sendPort);
    SendPort? answers;
    requests.listen((m) {
      if (m is SendPort) {
        answers = m;
        return;
      }
      final r = m as List;
      final id = r[0] as int;
      (int, String) out;
      try {
        out = switch (r[1] as String) {
          'open' => native.open(r[2] as String, r[3] as int),
          'memory' => native.memory(),
          'query' => native.query(r[2] as int, r[3] as String, r[4] as String, r[5] as Uint8List?),
          'changes' => native.changes(r[2] as int, r[3] as int),
          'syncStart' => native.syncStart(r[2] as int, r[3] as String),
          'schema' => native.schema(r[2] as int, r[3] as String, r[4] as int),
          'syncFeed' =>
            native.syncFeed(r[2] as int, r[3] as int, r[4] as int, r[5] as int, r[6] as int, r[7] as Uint8List?),
          'version' => (0, native.version),
          final op => native.byHandle(op, r[2] as int),
        };
      } catch (e) {
        out = (11, jsonEncode({'kind': 'error', 'message': 'internal error: $e'}));
      }
      answers!.send([id, out.$1, out.$2]);
    });
  }
}
