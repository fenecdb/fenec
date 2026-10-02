import 'dart:typed_data';

import 'fenec.dart';

/// A value as it goes into the parameters: a [DateTime] as the text
/// JavaScript's `toISOString` writes -- UTC, to the millisecond -- which the
/// JS builder sends a `Date` as, inside lists and maps too; a typed list as
/// it is. A map keeps its order, which is a document's.
Object? normalize(Object? v) => switch (v) {
      null || String() || bool() || num() || TypedData() => v,
      DateTime() => iso(v),
      Map() => {for (final e in v.entries) e.key.toString(): normalize(e.value)},
      Iterable() => [for (final x in v) normalize(x)],
      _ => throw FenecException(FenecCode.builder, 'this object cannot be used as a fenecdb value: ${v.runtimeType}'),
    };

/// A vector: a [Float32List] or a [Float64List], or a list of finite
/// numbers, which the engine reads as a vector either way.
List<double>? vectorOf(Object? v) {
  if (v is Float32List) return v.isEmpty ? null : v;
  if (v is Float64List) return v.isEmpty || v.any((x) => !x.isFinite) ? null : v;
  if (v is List && v.isNotEmpty && v.every((x) => x is num && x.isFinite)) {
    return [for (final x in v) (x as num).toDouble()];
  }
  return null;
}

/// JSON, as the engine reads it: a whole `double` as an integer, as
/// JavaScript writes one; JSON has no word for a NaN or an infinity, and a
/// typed field refuses the `null` it goes as.
String writeJson(Object? v) {
  final out = StringBuffer();
  _write(v, out);
  return out.toString();
}

void _write(Object? v, StringBuffer out) {
  switch (v) {
    case null:
      out.write('null');
    case String():
      quote(v, out);
    case bool() || int():
      out.write(v);
    case double():
      out.write(number(v));
    case TypedData() when v is List<num>:
      _list(v as List<num>, out);
    case List():
      _list(v, out);
    case Map():
      out.write('{');
      var first = true;
      for (final e in v.entries) {
        if (!first) out.write(',');
        first = false;
        quote(e.key.toString(), out);
        out.write(':');
        _write(e.value, out);
      }
      out.write('}');
    default:
      _write(normalize(v), out);
  }
}

void _list(List<Object?> v, StringBuffer out) {
  out.write('[');
  for (var i = 0; i < v.length; i++) {
    if (i > 0) out.write(',');
    _write(v[i], out);
  }
  out.write(']');
}

String number(double d) {
  if (!d.isFinite) return 'null';
  if (d == d.truncateToDouble() && d.abs() < 1e15) return d.toInt().toString();
  return d.toString();
}

void quote(String s, StringBuffer out) {
  out.write('"');
  for (final c in s.runes) {
    switch (c) {
      case 0x22:
        out.write(r'\"');
      case 0x5c:
        out.write(r'\\');
      case 0x0a:
        out.write(r'\n');
      case 0x0d:
        out.write(r'\r');
      case 0x09:
        out.write(r'\t');
      case 0x08:
        out.write(r'\b');
      case 0x0c:
        out.write(r'\f');
      default:
        if (c < 0x20) {
          out.write('\\u${c.toRadixString(16).padLeft(4, '0')}');
        } else {
          out.writeCharCode(c);
        }
    }
  }
  out.write('"');
}

/// A time as JavaScript's `toISOString` writes it: Dart's own writes the
/// microseconds where there are any, and the zone's offset for a local time.
String iso(DateTime t) {
  final u = t.toUtc();
  String pad(int n, int w) => n.toString().padLeft(w, '0');
  return '${pad(u.year, 4)}-${pad(u.month, 2)}-${pad(u.day, 2)}T${pad(u.hour, 2)}:${pad(u.minute, 2)}:'
      '${pad(u.second, 2)}.${pad(u.millisecond, 3)}Z';
}
