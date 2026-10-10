// The query builder: FenecQL text and its parameters from a chain of calls.
//
//     final rows = await db.from('docs')
//         .select(['title'])
//         .where('year', '>=', 2024)
//         .near('embed', vector, ef: 64)
//         .limit(5)
//         .rows();
//
// It makes the text web/fenec.js's builder makes of the same chain, to the
// byte, as the Python, Go, .NET, Swift and Kotlin builders do:
// integrations/builder-golden.json holds the chains and what each must make,
// and every builder's tests run it. Every value goes in as a parameter; a
// name -- a collection, a field, a path into a json field -- cannot, so
// names are checked against FenecQL's own rule, and that check is the
// injection boundary. A step the builder refuses throws as it is called
// (FenecCode.builder), its message the JS builder's.

import 'dart:convert' show jsonEncode;
import 'dart:typed_data';

import 'fenec.dart';
import 'values.dart';

FenecException _refuse(String message) => FenecException(FenecCode.builder, message);

/// A condition: what [Cond.or], [Cond.and], [Cond.not], [Cond.raw] and
/// [Cond.cmp] make. A `Map` is one too, the JS builder's object condition:
/// each field's value is equality, `null` is `is null`, and a map is its
/// operators -- `{'year': {'gte': 2024}}` -- in the map's order, which is the
/// text's.
class Cond {
  final Node _node;
  Cond._(this._node);

  /// Joins conditions with `or`; a map is an object condition.
  static Cond or(List<Object> conds) => Cond._(Node('or', items: conds.map(_toNode).toList()));

  /// Joins conditions with `and`: `where` already ands, so this is only needed inside [or].
  static Cond and(List<Object> conds) => Cond._(Node('and', items: conds.map(_toNode).toList()));

  /// Negates a condition.
  static Cond not(Object cond) => Cond._(Node('not', items: [_toNode(cond)]));

  /// What the builder cannot express (a function call): each `?` is bound to
  /// the next parameter -- a literal `?` goes in as one too.
  /// `Cond.raw('cosine(embed, ?) > ?', [vector, 0.5])`
  static Cond raw(String sql, [List<Object?> params = const []]) =>
      Cond._(Node('raw', sql: sql, values: params.map(normalize).toList()));

  /// One comparison, `where`'s three arguments as a condition.
  static Cond cmp(String field, String op, Object? value) => Cond._(_condOf(field, op, value));
}

/// A value a write works out over the row it writes, rendered as FenecQL
/// with its values as parameters: [Computed.inc] and [Computed.expr], as the
/// JS builder's `inc` and `expr`. An expression is a column too -- of
/// [Query.select], or a key of [Query.group] -- as are [Computed.bucket],
/// [Computed.countDistinct], [Computed.first] and [Computed.last], and [as]
/// names the column it answers under.
class Computed {
  final Object? _by;
  final String? _sql;
  final List<Object?> _params;
  final String? _name;
  Computed._(this._by, this._sql, this._params, [this._name]);

  /// `{'n': Computed.inc(1)}` in an update: the field plus [by], counting
  /// from 0 where it is null -- `n: coalesce(n, 0) + $1` -- worked out under
  /// the write lock, so increments from many clients all land.
  static Computed inc([Object? by = 1]) {
    if (!(by is num && by.isFinite)) throw _refuse('inc() takes a number: ${jsonEncode(by)}');
    return Computed._(by, null, const []);
  }

  /// A value as a FenecQL expression over the row, each `?` bound to the
  /// next parameter: `Computed.expr('now()')`, `Computed.expr('price * ?', [1.2])`.
  /// In a select list it is a column: `Computed.expr('sum(px * qty) / sum(qty)').as('vwap')`.
  static Computed expr(String sql, [List<Object?> params = const []]) => Computed._(null, sql, params);

  /// `bucket(field, interval)`: the start of the interval a timestamp falls
  /// in -- `'15m'`, `'1h'`, `'1d'`, `'1w'` (from a Monday), `'1mo'`, `'1y'`,
  /// in UTC -- for a select list or a group. The interval is written into
  /// the text, since it is a part of the statement's shape.
  static Computed bucket(String field, String interval) {
    if (!_interval.hasMatch(interval)) {
      throw _refuse("bucket() takes an interval such as '15m', '1h', '1d', '1w' or '1mo': ${_quote(interval)}");
    }
    return Computed._(null, 'bucket(${_pathOf(field)}, $interval)', const []);
  }

  /// `count(distinct field)`: how many distinct values the rows hold.
  static Computed countDistinct(String field) => Computed._(null, 'count(distinct ${_pathOf(field)})', const []);

  /// `distance(field, point)`: the metres from a point, `[lon, lat]`, to the
  /// row's -- `Computed.distance('loc', [13.4, 52.5]).as('m')`, what Redis's
  /// `GEOSEARCH ... WITHDIST` answers with.
  static Computed distance(String field, Object? point) =>
      Computed._(null, 'distance(${_pathOf(field)}, ?)', [point]);

  /// `first(field)`, or `first(field by key)`: the value of the row least by
  /// [by] -- by the order the rows were written without one -- that has a
  /// value; a bar's open is `Computed.first('px', 'at')`.
  static Computed first(String field, [String? by]) => _pick('first', field, by);

  /// `last(field [by key])`: as [first], the row greatest by [by].
  static Computed last(String field, [String? by]) => _pick('last', field, by);

  static Computed _pick(String fn, String field, String? by) =>
      Computed._(null, '$fn(${_pathOf(field)}${by == null ? '' : ' by ${_pathOf(by)}'})', const []);

  /// The name the column answers under: `select ... as <name>`.
  Computed as(String name) => Computed._(_by, _sql, _params, _identOf(name, 'column'));
}

// `15m`, `1h`, `1d`, `1w`, `3mo`, `1y`: what `bucket` takes.
final _interval = RegExp(r'^[1-9][0-9]*(ms|s|m|h|d|w|mo|y)$');

/// One key of a lookup's order: a field, `asc` or `desc`, and a collation
/// (`tr` or `und`).
class SortKey {
  final String field;
  final String direction;
  final String? collate;
  const SortKey(this.field, [this.direction = 'asc', this.collate]);
}

class Node {
  final String t; // and, or, not, null, in, cmp, within, dist, raw
  final List<Node> items;
  final String field;
  final String op;
  final Object? value;
  final List<Object?> values;
  final bool negated;
  final String sql;

  Node(this.t,
      {this.items = const [],
      this.field = '',
      this.op = '',
      this.value,
      this.values = const [],
      this.negated = false,
      this.sql = ''});
}

const _maxLookupDepth = 8;

const _ops = {
  '=': '=', 'eq': '=', //
  '!=': '!=', 'ne': '!=', 'neq': '!=',
  '<': '<', 'lt': '<',
  '<=': '<=', 'lte': '<=', 'le': '<=',
  '>': '>', 'gt': '>',
  '>=': '>=', 'gte': '>=', 'ge': '>=',
  '~': '~', 'like': '~', 'contains': '~',
  'has': 'has',
  'in': 'in',
};

// FenecQL's identifier: the lexer's rule, as the JS builder writes it.
final _ident = RegExp(r'^[\p{Alphabetic}_][\p{Alphabetic}\p{N}_]*$', unicode: true);
final _path =
    RegExp(r'^[\p{Alphabetic}_][\p{Alphabetic}\p{N}_]*(\.[\p{Alphabetic}_][\p{Alphabetic}\p{N}_]*)*$', unicode: true);

String _quote(String s) {
  final out = StringBuffer();
  quote(s, out);
  return out.toString();
}

String _identOf(String name, [String what = 'field']) =>
    _ident.hasMatch(name) ? name : throw _refuse('invalid $what name: ${_quote(name)}');

/// A field's name, or a path into a json field: `meta.lang`.
String _pathOf(String name) => _path.hasMatch(name) ? name : throw _refuse('invalid field name: ${_quote(name)}');

// The JS builder's aggregate pattern, case-blind over ASCII as JavaScript's
// `i` flag without `u` is.
const _names = r'[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*';
final _aggregate = RegExp(
    r'^(count)\(\*?\)$|^count\(\s*distinct\s+(' +
        _names +
        r')\s*\)$|^(sum|avg|min|max|first|last)\((' +
        _names +
        r')\)$',
    caseSensitive: false);

// What makes an expression an aggregate's: a call of one.
final _aggregateCall = RegExp(r'\b(count|sum|avg|min|max|first|last)\s*\(', caseSensitive: false);

/// What JavaScript's `String.prototype.trim` takes off. Dart's own `trim`
/// takes Unicode's White_Space, which holds U+0085 where JavaScript's does
/// not.
bool _jsSpace(int c) =>
    const [0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x20, 0xa0, 0x1680, 0x2028, 0x2029, 0x202f, 0x205f, 0x3000, 0xfeff]
        .contains(c) ||
    (c >= 0x2000 && c <= 0x200a);

String _jsTrim(String s) {
  var a = 0, b = s.length;
  while (a < b && _jsSpace(s.codeUnitAt(a))) {
    a++;
  }
  while (b > a && _jsSpace(s.codeUnitAt(b - 1))) {
    b--;
  }
  return s.substring(a, b);
}

/// A select item's name: a field, or an aggregate of one spelled as
/// FenecQL spells it -- `count(*)`, `count(distinct user)`, `sum(total)`,
/// `avg(f)`, `min(f)`, `max(f)`, `first(f)`, `last(f)`, a path where a field
/// goes -- answering under that name.
(String, bool) _columnName(String name) {
  final m = _aggregate.firstMatch(_jsTrim(name));
  if (m == null) return (_pathOf(name), false);
  if (m[1] != null) return ('count(*)', true);
  if (m[2] != null) return ('count(distinct ${m[2]})', true);
  return ('${m[3]!.toLowerCase()}(${m[4]})', true);
}

/// A select item: a name, or an expression -- an aggregate's when it calls one.
(Object, bool) _column(Object c) => switch (c) {
      String() => _columnName(c),
      Computed(_sql: final String sql) => (c, _aggregateCall.hasMatch(sql)),
      _ => throw _refuse('a column is a name or an expression, and inc() is neither'),
    };

/// A group key: a field or a path, a name the list gives a column, or an expression.
Object _groupKey(Object k) => switch (k) {
      String() => _pathOf(k),
      Computed(_sql: String()) => k,
      _ => throw _refuse('a group key is a name or an expression, and inc() is neither'),
    };

/// An expression's text, each `?` bound to the next of its parameters.
String _exprText(Computed v, String Function(Object?) bind) {
  final pieces = v._sql!.split('?');
  final out = StringBuffer();
  for (var i = 0; i < pieces.length - 1; i++) {
    if (i >= v._params.length) throw _refuse('expr(): more `?` placeholders than parameters');
    out
      ..write(pieces[i])
      ..write(bind(v._params[i]));
  }
  if (pieces.length - 1 != v._params.length) throw _refuse('expr(): too many parameters given');
  return (out..write(pieces.last)).toString();
}

/// A select item or a group key as text, an expression's values bound.
String _columnText(Object c, String Function(Object?) bind) {
  if (c is! Computed) return c as String;
  final text = _exprText(c, bind);
  return c._name == null ? text : '$text as ${c._name}';
}

bool _direction(String dir) => switch (dir.toLowerCase()) {
      'asc' => true,
      'desc' => false,
      _ => throw _refuse("order direction must be 'asc' or 'desc': $dir"),
    };

String? _collation(String? name) => switch (name) {
      null || 'und' || 'tr' => name,
      _ => throw _refuse("unknown collation: ${_quote(name)}; there are 'und' and 'tr'"),
    };

/// A facet clause: its field, `top`, range bounds as written and whether it
/// is disjunctive.
typedef _Facet = (String field, int? top, List<String>? ranges, bool disjunctive);

/// A number as JavaScript's `String` writes it, the text every other
/// builder makes: Dart writes a double as JavaScript does but for the `.0`
/// of a whole one (2500.0).
String _jsNumber(num n) {
  if (n is double && n == n.truncateToDouble() && n.abs() < 1e21) return BigInt.from(n).toString();
  return n.toString();
}

/// `limit`, `offset`, `ef` and the rest are literals, never parameters: a
/// whole number JavaScript holds exactly.
int _whole(int n, String what) =>
    n >= 0 && n <= 9007199254740991 ? n : throw _refuse('$what must be a non-negative integer: $n');

Node _toNode(Object c) => switch (c) {
      Cond() => c._node,
      Map() => _objectCond(c),
      _ => throw _refuse('expected an object as a condition: ${writeJson(normalize(c))}'),
    };

Node _objectCond(Map<Object?, Object?> obj) {
  final items = [for (final e in obj.entries) _fieldCond(_pathOf(e.key.toString()), e.value)];
  return items.length == 1 ? items[0] : Node('and', items: items);
}

Node _condOf(String field, String op, Object? value) {
  final o = _ops[op] ?? (throw _refuse('unknown operator `$op`'));
  final f = _pathOf(field);
  return o == 'in' ? _inCond(f, value) : _cmp(f, o, value);
}

/// One field's condition: a value is equality, null is null, a map its operators.
Node _fieldCond(String field, Object? spec) {
  if (spec == null) return Node('null', field: field);
  if (spec is! Map) return _cmp(field, '=', spec);
  final items = <Node>[];
  for (final e in spec.entries) {
    final k = e.key.toString();
    if (k == 'not') {
      items.add(e.value == null
          ? Node('null', field: field, negated: true)
          : Node('not', items: [_fieldCond(field, e.value)]));
      continue;
    }
    // A point's: in the box `[west, south, east, north]`, and the metres
    // from a point, compared.
    if (k == 'within') {
      items.add(Node('within', field: field, value: e.value));
      continue;
    }
    if (k == 'distance') {
      items.add(_distanceCond(field, e.value));
      continue;
    }
    final op = _ops[k] ?? (throw _refuse('unknown operator `$k` (field: $field)'));
    items.add(op == 'in' ? _inCond(field, e.value) : _cmp(field, op, e.value));
  }
  return switch (items.length) {
    0 => throw _refuse('empty condition object (field: $field)'),
    1 => items[0],
    _ => Node('and', items: items),
  };
}

/// `{'from': [lon, lat], 'lte': 500}`: a point's distance, compared --
/// `distance(loc, $1) <= $2`, each comparison given its own.
Node _distanceCond(String field, Object? spec) {
  final from = spec is Map ? spec['from'] : null;
  if (spec is! Map || from is! List || from is TypedData) {
    throw _refuse('distance takes { from: [lon, lat], <op>: metres } (field: $field)');
  }
  final items = <Node>[];
  for (final e in spec.entries) {
    final k = e.key.toString();
    if (k == 'from') continue;
    final op = _ops[k];
    if (op == null || op == 'in' || op == 'has' || op == '~' || e.value is! num) {
      throw _refuse('distance compares metres with lt, lte, gt, gte, eq or ne: $k (field: $field)');
    }
    items.add(Node('dist', field: field, op: op, value: e.value, values: [from]));
  }
  return switch (items.length) {
    0 => throw _refuse('distance needs a comparison, as lte: metres (field: $field)'),
    1 => items[0],
    _ => Node('and', items: items),
  };
}

Node _inCond(String field, Object? values) {
  // A typed list is no array to JavaScript's Array.isArray either.
  if (values is! List || values is TypedData) throw _refuse('`in` expects an array (field: $field)');
  if (values.isEmpty) throw _refuse('`in` does not accept an empty array (field: $field)');
  return Node('in', field: field, values: values);
}

/// `= null` is never true in FenecQL; what is meant is `is null`.
Node _cmp(String field, String op, Object? value) {
  if (value != null) return Node('cmp', field: field, op: op, value: value);
  return switch (op) {
    '=' => Node('null', field: field),
    '!=' => Node('null', field: field, negated: true),
    _ => throw _refuse('`$op` cannot be used with null (field: $field)'),
  };
}

/// Flattens empty and single-child junctions before rendering: the
/// parentheses depend on the child count, and rendering binds parameters.
Node? _prune(Node c) {
  if (c.t == 'and' || c.t == 'or') {
    final items = c.items.map(_prune).whereType<Node>().toList();
    if (items.isEmpty) return null;
    return items.length == 1 ? items[0] : Node(c.t, items: items);
  }
  if (c.t == 'not') {
    final item = _prune(c.items[0]);
    return item == null ? null : Node('not', items: [item]);
  }
  return c;
}

String _render(Node c, String Function(Object?) bind, [String? parent]) {
  switch (c.t) {
    case 'and' || 'or':
      final s = c.items.map((x) => _render(x, bind, c.t)).join(' ${c.t} ');
      // `and` binds tighter than `or`: one inside the other needs parens.
      return parent != null && parent != c.t ? '($s)' : s;
    case 'not':
      return 'not (${_render(c.items[0], bind)})';
    case 'null':
      return '${c.field} is ${c.negated ? 'not ' : ''}null';
    case 'in':
      return '${c.field} in [${c.values.map(bind).join(', ')}]';
    case 'cmp':
      return '${c.field} ${c.op} ${bind(c.value)}';
    case 'within':
      return 'within(${c.field}, ${bind(c.value)})';
    case 'dist':
      final point = bind(c.values[0]);
      return 'distance(${c.field}, $point) ${c.op} ${bind(c.value)}';
  }
  final pieces = c.sql.split('?');
  final out = StringBuffer();
  for (var i = 0; i < pieces.length - 1; i++) {
    if (i >= c.values.length) throw _refuse('raw(): more `?` placeholders than parameters');
    out
      ..write(pieces[i])
      ..write(bind(c.values[i]));
  }
  if (pieces.length - 1 != c.values.length) throw _refuse('raw(): too many parameters given');
  return (out..write(pieces.last)).toString();
}

bool _readsMore(Node c) => switch (c.t) {
      'and' || 'or' || 'not' => c.items.any(_readsMore),
      'raw' => RegExp(r'\bget\b', caseSensitive: false).hasMatch(c.sql),
      _ => false,
    };

/// A statement and its parameters, as they would be run.
typedef Statement = ({String text, List<Object?> params});

typedef Exec = Future<Answer> Function(String text, List<Object?> params);

class _Vector {
  final String field;
  final Object? vector;
  final int? n;
  final bool exact;
  _Vector(this.field, this.vector, this.n, this.exact);
}

class _Key {
  final String field;
  final bool asc;
  final String? collate;
  _Key(this.field, this.asc, this.collate);
}

class _Level {
  final String collection, on;
  final String? parent;
  final List<String>? project;
  final List<Node> cond;
  final bool required;
  final List<_Key> order;
  final int? limit;
  final int offset;
  _Level(this.collection, this.on, this.parent, this.project, this.cond, this.required, this.order, this.limit,
      this.offset);
}

/// A `highlight` (no [words]) or a `snippet` in the select list.
class _Mark {
  final String field;
  final int? words;
  final String? ellipsis, pre, post;
  _Mark(this.field, this.words, this.ellipsis, this.pre, this.post);
  String get kind => words == null ? 'highlight' : 'snippet';
}

/// A mark's tags: both or neither, each text. Taken as any value so that a
/// value read out of JSON is refused with the JS builder's message, not a
/// cast error.
(String?, String?) _tags(Object? pre, Object? post, String what) {
  if (pre == null && post == null) return (null, null);
  if (pre == null || post == null) throw _refuse('$what takes both pre and post, or neither');
  return (_textOf(pre, '$what pre'), _textOf(post, '$what post'));
}

String _textOf(Object? v, String what) =>
    v is String ? v : throw _refuse('$what must be text: ${writeJson(normalize(v))}');

/// A query over one collection, made by [Fenec.from] or [Query.from].
/// Immutable: each call hands back a new one, so a base query can be kept
/// and branched from.
class Query {
  final String collection;
  final Exec? _exec;
  final List<Object>? _project; // each a name or a Computed
  final bool _aggregate;
  final List<Object>? _group; // each a name or a Computed
  final List<Node> _cond;
  final _Vector? _near;
  final (String, String)? _match;
  final _Vector? _rerank;
  final (int?, int?)? _fuse;
  final List<_Key> _order;
  final int? _limit;
  final int _offset;
  // ` require n`, or none.
  final String _require;
  final bool _count;
  final List<_Level> _lookups;
  final List<_Mark> _marks;
  final List<_Facet> _facets;

  Query._(this.collection,
      {Exec? exec,
      List<Object>? project,
      bool aggregate = false,
      List<Object>? group,
      List<Node> cond = const [],
      _Vector? near,
      (String, String)? match,
      _Vector? rerank,
      (int?, int?)? fuse,
      List<_Key> order = const [],
      int? limit,
      int offset = 0,
      String require = '',
      bool count = false,
      List<_Level> lookups = const [],
      List<_Mark> marks = const [],
      List<_Facet> facets = const []})
      : _exec = exec,
        _project = project,
        _aggregate = aggregate,
        _group = group,
        _cond = cond,
        _near = near,
        _match = match,
        _rerank = rerank,
        _fuse = fuse,
        _order = order,
        _limit = limit,
        _offset = offset,
        _require = require,
        _count = count,
        _lookups = lookups,
        _marks = marks,
        _facets = facets;

  /// A query bound to no database: for its text alone, [toFenecQL].
  static Query from(String collection) => Query._(_identOf(collection, 'collection'));

  // A copy with what changes; `_keep` tells a field left as it is from one
  // set to null.
  static const _keep = Object();

  Query _with({
    Object? exec = _keep,
    Object? project = _keep,
    bool? aggregate,
    Object? group = _keep,
    List<Node>? cond,
    Object? near = _keep,
    Object? match = _keep,
    Object? rerank = _keep,
    Object? fuse = _keep,
    List<_Key>? order,
    Object? limit = _keep,
    int? offset,
    String? require,
    bool? count,
    List<_Level>? lookups,
    List<_Mark>? marks,
    List<_Facet>? facets,
  }) =>
      Query._(collection,
          exec: identical(exec, _keep) ? _exec : exec as Exec?,
          project: identical(project, _keep) ? _project : project as List<Object>?,
          aggregate: aggregate ?? _aggregate,
          group: identical(group, _keep) ? _group : group as List<Object>?,
          cond: cond ?? _cond,
          near: identical(near, _keep) ? _near : near as _Vector?,
          match: identical(match, _keep) ? _match : match as (String, String)?,
          rerank: identical(rerank, _keep) ? _rerank : rerank as _Vector?,
          fuse: identical(fuse, _keep) ? _fuse : fuse as (int?, int?)?,
          order: order ?? _order,
          limit: identical(limit, _keep) ? _limit : limit as int?,
          offset: offset ?? _offset,
          require: require ?? _require,
          count: count ?? _count,
          lookups: lookups ?? _lookups,
          marks: marks ?? _marks,
          facets: facets ?? _facets);

  /// The query bound to whatever runs a text and its parameters.
  Query bind(Exec exec) => _with(exec: exec);

  /// Every collection the query reads -- its own and each lookup's -- or null
  /// when a `raw` fragment may read one more: a live query runs again when
  /// one of them is written.
  List<String>? get reads {
    if (_cond.any(_readsMore) || _lookups.any((l) => l.cond.any(_readsMore))) return null;
    return {collection, ..._lookups.map((l) => l.collection)}.toList();
  }

  /// `select a, b`; none, or `'*'`, is every field. Aggregates go in the same
  /// list as FenecQL spells them and answer under that name:
  /// `select(['status', 'count(*)', 'sum(total)']).group('status')`. An
  /// expression -- [Computed.expr], [Computed.bucket],
  /// [Computed.countDistinct], [Computed.first], [Computed.last] -- answers
  /// under the name its [Computed.as] gives it:
  /// `select(['sym', Computed.expr('sum(px * qty) / sum(qty)').as('vwap')]).group('sym')`.
  Query select(List<Object> columns) {
    if (columns.isEmpty || columns.contains('*')) return _with(project: null, aggregate: false);
    final cs = columns.map(_column).toList();
    return _with(project: [for (final c in cs) c.$1], aggregate: cs.any((c) => c.$2));
  }

  /// `group a, b`: a row per distinct set of the keys' values, for a select
  /// list that aggregates. [keys] is one key or a list of them, each a field
  /// or a path, a name the list gives a column with [Computed.as], or an
  /// expression: `group(['sym', Computed.bucket('at', '1h')])`.
  Query group(Object keys) {
    final list = keys is List ? keys.cast<Object>() : [keys];
    if (list.isEmpty) throw _refuse('group takes at least one key');
    return _with(group: list.map(_groupKey).toList());
  }

  /// `where(field, op, value)`, `where(field, spec)` or `where(condition)`,
  /// joined to the conditions before with `and`. The op is a symbol or its
  /// word: `=`, `!=`, `<`, `<=`, `>`, `>=`, `~`, `has`, `in` (a list), or
  /// `eq`, `ne`, `lt`, `gte`, `like`, `contains` ... A null with `=` or `!=`
  /// is `is null` and `is not null`. A spec is a value (equality), null, or
  /// a map of operators; a condition a [Cond] or a map of fields.
  Query where(Object fieldOrCond, [Object? opOrSpec = _none, Object? value = _none]) =>
      _with(cond: [..._cond, _condOfArgs(fieldOrCond, opOrSpec, value)]);

  /// Everything conditioned so far, or the condition `where` would add.
  Query orWhere(Object fieldOrCond, [Object? opOrSpec = _none, Object? value = _none]) {
    final right = _condOfArgs(fieldOrCond, opOrSpec, value);
    if (_cond.isEmpty) return _with(cond: [right]);
    return _with(cond: [
      Node('or', items: [Node('and', items: _cond), right])
    ]);
  }

  static const _none = Object();

  static Node _condOfArgs(Object a, Object? b, Object? c) {
    if (identical(b, _none)) return _toNode(a);
    if (identical(c, _none)) return _fieldCond(_pathOf(a as String), b);
    return _condOf(a as String, b as String, c);
  }

  /// `near field $n [ef N] [exact]`. A [Float32List] goes to the library as its bytes.
  Query near(String field, Object? vector, {int? ef, bool exact = false}) =>
      _with(near: _Vector(_identOf(field), vector, ef == null ? null : _whole(ef, 'ef'), exact));

  /// `match field $n`: BM25 over a `@text` index.
  Query match(String field, String query) => _with(match: (_identOf(field), query));

  /// `fuse [k N] [candidates N]`: with both [match] and [near], ranks by both.
  Query fuse({int? k, int? candidates}) =>
      _with(fuse: (k == null ? null : _whole(k, 'k'), candidates == null ? null : _whole(candidates, 'candidates')));

  /// `rerank field $n [candidates N]`: reorders what [match] found by exact distance.
  Query rerank(String field, Object? vector, {int? candidates}) => _with(
      rerank: _Vector(_identOf(field), vector, candidates == null ? null : _whole(candidates, 'candidates'), false));

  /// `highlight(field)` in the select list: where the terms [match] found
  /// stand in the field's text -- `[start, end]` pairs of UTF-16 offsets,
  /// which a Dart `String` indexes by -- or, given [pre] and [post], the
  /// text with each mark between them. The text is not escaped. Answers
  /// under `highlight(field)`, after the fields [select] named.
  ///
  ///     db.from('docs').select(['title']).highlight('body', pre: '<mark>', post: '</mark>')
  ///         .match('body', text)
  Query highlight(String field, {Object? pre, Object? post}) {
    final (p, q) = _tags(pre, post, 'highlight');
    return _mark(_Mark(_identOf(field), null, null, p, q));
  }

  /// `snippet(field, words)`: the window of [words] words around the
  /// densest marks, `{text, marks}` -- or the marked text, given [pre] and
  /// [post] -- with [ellipsis] where it leaves text out. Answers under
  /// `snippet(field)`.
  Query snippet(String field, int words, {Object? ellipsis, Object? pre, Object? post}) {
    final f = _identOf(field);
    final n = _whole(words, 'snippet words');
    final (p, q) = _tags(pre, post, 'snippet');
    if (n == 0) throw _refuse('snippet shows at least one word');
    final e = ellipsis == null ? null : _textOf(ellipsis, 'snippet ellipsis');
    return _mark(_Mark(f, n, e, p, q));
  }

  Query _mark(_Mark mark) {
    // Each answers under its label, and a row holds a name once.
    if (_marks.any((m) => m.kind == mark.kind && m.field == mark.field)) {
      throw _refuse('${mark.kind}(${mark.field}) is asked twice');
    }
    return _with(marks: [..._marks, mark]);
  }

  /// `facet field [top N]`: each value the field -- or a path into a json
  /// field -- holds over every row the query matches, not only the page,
  /// and how many rows hold it, most first; [top] keeps the commonest. The
  /// counts come back beside the rows: [answer]'s [Answer.facets], and
  /// [rows]'s [Rows.facets].
  ///
  ///     db.from('products').match('title', 'phone').where('price', '<', 500)
  ///         .facet('brand', top: 10).facet('color').limit(20)
  ///
  /// [ranges] counts the rows in each range of numbers from one bound up to
  /// the next, every range in order, its value `[from, to]`; [disjunctive]
  /// -- `true` or `false` -- counts as if the filter's own conditions on the
  /// field were not there, so the other values a shopper could add are
  /// counted too. Both are taken as any value, as the JS builder takes
  /// them, so one of another type is refused by its message.
  Query facet(String field, {int? top, List<Object?>? ranges, Object? disjunctive}) {
    final f = _pathOf(field);
    final n = top == null ? null : _whole(top, 'facet top');
    if (n == 0) throw _refuse('facet $f top 0 answers nothing');
    if (_facets.any((g) => g.$1 == f)) throw _refuse('facet $f is asked twice');
    List<String>? bounds;
    if (ranges != null) {
      // The engine's rule, refused before anything is sent.
      var ok = n == null && ranges.length >= 2;
      for (var i = 0; ok && i < ranges.length; i++) {
        final b = ranges[i];
        ok = b is num && b.isFinite && (i == 0 || b > (ranges[i - 1] as num));
      }
      if (!ok) {
        throw _refuse(
            'facet $f ranges takes 2 to 10 001 numbers, each above the one before, and no top: every range answers, in order');
      }
      bounds = [for (final b in ranges) _jsNumber(b as num)];
    }
    if (disjunctive != null && disjunctive is! bool) throw _refuse('facet $f disjunctive is true or false');
    return _with(facets: [..._facets, (f, n, bounds, disjunctive == true)]);
  }

  /// `lookup name on child [= parent] ...`: each row's children, attached to
  /// it. [on] names the child's field, [parentKey] the parent's (`id` unless
  /// given); the rest binds to the looked-up collection, and [limit] counts
  /// children per parent. [required] drops a parent no child matches.
  /// Called again, it chains onto the collection the call before named.
  Query lookup(String name,
      {required String? on,
      String? parentKey,
      List<String>? select,
      Object? where,
      bool required = false,
      List<SortKey> order = const [],
      int? limit,
      int? offset}) {
    if (on == null || on.isEmpty) throw _refuse('lookup needs `on`: the child field holding the key');
    final level = _Level(
      _identOf(name, 'collection'),
      _identOf(on),
      parentKey == null ? null : _identOf(parentKey),
      select == null || select.contains('*') ? null : select.map(_pathOf).toList(),
      where == null ? const [] : [_toNode(where)],
      required,
      [for (final k in order) _Key(_pathOf(k.field), _direction(k.direction), _collation(k.collate))],
      limit == null ? null : _whole(limit, 'limit'),
      offset == null ? 0 : _whole(offset, 'offset'),
    );
    return _with(lookups: [..._lookups, level]);
  }

  /// `order field asc|desc`: each call adds a key. `collate: 'tr'` puts text
  /// in Turkish order, `'und'` in Unicode's root order.
  Query order(String field, [String direction = 'asc', String? collate]) =>
      // Over groups a key may be an aggregate of the list, by its name.
      _with(order: [..._order, _Key(_columnName(field).$1, _direction(direction), _collation(collate))]);

  /// How many rows come back.
  Query limit(int n) => _with(limit: _whole(n, 'limit'));

  /// How many rows are passed over first.
  Query offset(int n) => _with(offset: _whole(n, 'offset'));

  /// `require n` on a read: the rows it answers, after [limit], must number
  /// n, or it is refused (412, unmet) and the batch it is in put back, as a
  /// write's `require` is -- a checkout's guard on a read.
  Query require(int n) => _with(require: _requireClause(n));

  // ------------------------------------------------------------ the text

  /// The statement and its parameters, as they would be run.
  Statement toFenecQL() {
    final params = <Object?>[];
    return (text: _text(_binder(params)), params: params);
  }

  static String Function(Object?) _binder(List<Object?> params) => (v) {
        params.add(normalize(v));
        return '\$${params.length}';
      };

  String _text(String Function(Object?) bind) {
    // The engine refuses each of these too; failing here runs nothing.
    if (_group != null && !_aggregate) {
      final keys = _group.map((k) => k is Computed ? k._sql : k).join(', ');
      throw _refuse("group $keys needs an aggregate in select: 'count(*)'");
    }
    if (_aggregate) {
      final clash = _near != null
          ? 'near'
          : _match != null
              ? 'match'
              : _lookups.isNotEmpty
                  ? 'lookup'
                  : _count
                      ? 'count'
                      : null;
      if (clash != null) throw _refuse('aggregates cannot be combined with $clash');
      if (_group == null && (_order.isNotEmpty || _limit != null || _offset > 0)) {
        throw _refuse('aggregates answer one row; group makes a row per value');
      }
      if (_group == null && _require.isNotEmpty) {
        throw _refuse('require counts the rows a query answers, and an aggregate answers one');
      }
    }
    if (_marks.isNotEmpty) {
      final what = _marks[0].kind;
      if (_match == null) throw _refuse('$what needs match: it marks the terms match found');
      if (_aggregate) throw _refuse("$what marks a row's text; aggregates answer groups");
    }
    if (_facets.isNotEmpty) {
      if (_near != null) {
        throw _refuse(
            'facet counts the rows a filter or match selects, and near ranks every row: ask the facets without near');
      }
      if (_aggregate) throw _refuse('facet cannot be combined with aggregates: group counts by value');
    }
    if (_rerank != null && _match == null) throw _refuse('rerank needs match: it reorders what match found');
    if (_match != null && _near != null && _fuse == null) {
      throw _refuse('match and near cannot be combined: both order the result; fuse() ranks by both');
    }
    if (_fuse != null && (_match == null || _near == null)) {
      throw _refuse('fuse combines match and near: the query needs both');
    }
    if (_fuse != null && _rerank != null) {
      throw _refuse('fuse and rerank are two ways to use a vector with match: pick one');
    }
    if (_lookups.isNotEmpty) {
      final clash = _near != null
          ? 'near'
          : _match != null
              ? 'match'
              : _rerank != null
                  ? 'rerank'
                  : null;
      if (clash != null) throw _refuse('lookup cannot be combined with $clash');
      if (_count && !_lookups[0].required) {
        throw _refuse('count cannot be used with lookup unless it is required: there is nothing to attach children to');
      }
      if (_lookups.length > _maxLookupDepth) {
        throw _refuse('lookup chained too deep: at most $_maxLookupDepth levels');
      }
      final seen = [collection];
      for (final l in _lookups) {
        if (seen.contains(l.collection)) {
          throw _refuse('${l.collection} cannot look itself up: both sides would answer to the same name');
        }
        seen.add(l.collection);
      }
    }
    if (_count) {
      final extra = _extraClause() ?? (_require.isNotEmpty ? 'require' : null);
      if (extra != null) throw _refuse('count cannot be used with `$extra`');
    }

    final sql = StringBuffer('get $collection');
    // The marks after the fields `select` named, or after every field, each
    // binding its ellipsis and tags in the order written.
    final items = [for (final m in _marks) _markText(m, bind)];
    // The list's values are bound after the marks', as the JS builder binds
    // them: a parameter is named by its number, so either order reads alike.
    final cols = _project?.map((c) => _columnText(c, bind)).toList();
    final select = [
      ...?cols ?? (items.isEmpty ? null : const ['*']),
      ...items
    ];
    if (select.isNotEmpty) sql.write(' select ${select.join(', ')}');
    final w = _whereOf(_cond, bind);
    if (w != null) sql.write(' where $w');
    if (_group != null) sql.write(' group ${_group.map((k) => _columnText(k, bind)).join(', ')}');
    if (_near != null) {
      sql.write(' near ${_near.field} ${bind(_near.vector)}');
      if (_near.n != null) sql.write(' ef ${_near.n}');
      if (_near.exact) sql.write(' exact');
    }
    if (_match != null) sql.write(' match ${_match.$1} ${bind(_match.$2)}');
    if (_rerank != null) {
      sql.write(' rerank ${_rerank.field} ${bind(_rerank.vector)}');
      if (_rerank.n != null) sql.write(' candidates ${_rerank.n}');
    }
    if (_fuse != null) {
      sql.write(' fuse');
      if (_fuse.$1 != null) sql.write(' k ${_fuse.$1}');
      if (_fuse.$2 != null) sql.write(' candidates ${_fuse.$2}');
    }
    _orderText(sql, _order);
    if (_limit != null) sql.write(' limit $_limit');
    if (_offset > 0) sql.write(' offset $_offset');
    // Before a lookup, whose clauses are the children's.
    sql.write(_require);
    if (_count) sql.write(' count');
    if (_facets.isNotEmpty) {
      sql.write(' facet ${_facets.map((f) => [
            f.$1,
            if (f.$2 != null) ' top ${f.$2}',
            if (f.$3 != null) ' ranges [${f.$3!.join(', ')}]',
            if (f.$4) ' disjunctive',
          ].join()).join(', ')}');
    }
    // Terminal, so every clause after it is the child's -- and last, so its
    // parameters come after the parent's.
    for (final l in _lookups) {
      sql.write(' lookup ${l.collection} on ${l.on}');
      if (l.parent != null) sql.write(' = ${l.parent}');
      if (l.required) sql.write(' required');
      if (l.project != null) sql.write(' select ${l.project!.join(', ')}');
      final lw = _whereOf(l.cond, bind);
      if (lw != null) sql.write(' where $lw');
      _orderText(sql, l.order);
      if (l.limit != null) sql.write(' limit ${l.limit}');
      if (l.offset > 0) sql.write(' offset ${l.offset}');
    }
    return sql.toString();
  }

  static String _markText(_Mark m, String Function(Object?) bind) {
    final s = StringBuffer('${m.kind}(${m.field}');
    if (m.words != null) s.write(', ${m.words}');
    // A snippet's tags come after its ellipsis: tags alone bind '' for it.
    if (m.ellipsis != null || (m.words != null && m.pre != null)) s.write(', ${bind(m.ellipsis ?? '')}');
    if (m.pre != null) s.write(', ${bind(m.pre)}, ${bind(m.post)}');
    return (s..write(')')).toString();
  }

  static void _orderText(StringBuffer sql, List<_Key> keys) {
    for (var i = 0; i < keys.length; i++) {
      final k = keys[i];
      sql.write(i == 0 ? ' order ' : ', ');
      sql.write(k.field);
      if (k.collate != null) sql.write(' collate ${k.collate}');
      sql.write(k.asc ? ' asc' : ' desc');
    }
  }

  static String? _whereOf(List<Node> cond, String Function(Object?) bind) {
    final root = _prune(Node('and', items: cond));
    return root == null ? null : _render(root, bind);
  }

  String? _extraClause({bool picks = false}) => _near != null
      ? 'near'
      : _match != null
          ? 'match'
          : _rerank != null
              ? 'rerank'
              : _order.isNotEmpty && !picks
                  ? 'order'
                  : _limit != null && !picks
                      ? 'limit'
                      : _offset > 0
                          ? 'offset'
                          : _project != null
                              ? 'select'
                              : null;

  // Near, offset, select mean something only to a read; dropped from a
  // write, offset(1).delete() would delete from the first row. Order and
  // limit pick the rows an update or a delete writes.
  void _assertPlain(String verb) {
    if (_require.isNotEmpty) throw _refuse('$verb takes require as its option: $verb(..., { require: n })');
    final extra = _extraClause(picks: verb == 'update' || verb == 'delete');
    if (extra != null) throw _refuse('$verb cannot be used with `$extra`');
    if (_lookups.isNotEmpty) throw _refuse('$verb cannot be used with `lookup`');
    if (_facets.isNotEmpty) throw _refuse('$verb cannot be used with `facet`');
    if ((verb == 'insert' || verb == 'upsert') && _cond.isNotEmpty) throw _refuse('$verb cannot be used with `where`');
  }

  // An update or delete of every row is too easy to do by accident and
  // cannot be undone: it has to be asked for, with all. A limit bounds it.
  String _requireFilter(String verb, bool all, String Function(Object?) bind) {
    final w = _whereOf(_cond, bind);
    if (w != null) return ' where $w';
    if (all || _limit != null) return '';
    throw _refuse('an unfiltered $verb covers the whole collection; if you mean it, $verb({ all: true })');
  }

  // `require n`: the write is refused, and put back whole, unless it
  // wrote exactly n rows -- a check and its write in one statement.
  static String _requireClause(int? n) {
    if (n == null) return '';
    if (n < 0) throw _refuse('require takes a count of rows, a whole number from 0 (got $n)');
    return ' require $n';
  }

  // An update's or a delete's ` order ... limit n returning ...`: the rows
  // it picks, and whether it answers them, under the fields named -- `*`
  // for every one.
  String _pick(List<String>? returning) {
    final sql = StringBuffer();
    _orderText(sql, _order);
    if (_limit != null) sql.write(' limit $_limit');
    if (returning == null) return sql.toString();
    if (returning.isEmpty) throw _refuse('returning names the fields it answers, or * for every one');
    if (returning.contains('*')) {
      if (returning.length > 1) throw _refuse('returning * answers every field: it takes no other');
      return (sql..write(' returning *')).toString();
    }
    return (sql..write(' returning ${returning.map(_pathOf).join(', ')}')).toString();
  }

  static String _renderDoc(Object? doc, String Function(Object?) bind, {bool insert = false}) {
    if (doc is! Map) throw _refuse('expected a document object');
    if (doc.isEmpty) throw _refuse('cannot write an empty document');
    return '{${doc.entries.map((e) {
      final key = _pathOf(e.key.toString());
      return '$key: ${_value(key, e.value, bind, insert)}';
    }).join(', ')}}';
  }

  /// A document's value: [Computed]'s text, or a parameter.
  static String _value(String key, Object? v, String Function(Object?) bind, bool insert) {
    if (v is! Computed) return bind(v);
    if (v._sql == null) {
      if (insert) throw _refuse('inc() reads the row it changes: use it in update (field: $key)');
      return 'coalesce($key, 0) + ${bind(v._by)}';
    }
    return _exprText(v, bind);
  }

  /// The `put` of a document -- a map of fields, in its order -- or a list of
  /// them, not run. [ifAbsent]: `put ... if absent`, which passes over a
  /// document whose id or `@unique` value a row holds and counts only what
  /// it wrote -- a lock taken, or not, in one statement. [require]: refused,
  /// and nothing written, unless it wrote exactly that many rows.
  Statement toInsert(Object docs, {bool ifAbsent = false, int? require}) {
    _assertPlain('insert');
    final list = docs is List ? docs : [docs];
    if (list.isEmpty) throw _refuse('cannot write an empty document list');
    final params = <Object?>[];
    final bind = _binder(params);
    final body = list.map((d) => _renderDoc(d, bind, insert: true)).join(', ');
    final absent = ifAbsent ? ' if absent' : '';
    final required = _requireClause(require);
    return (text: 'put $collection ${list.length == 1 ? body : '[$body]'}$absent$required', params: params);
  }

  /// The `set` of the rows the filter names, not run; with no filter it is
  /// refused unless [all] or a limit bounds it; with [require], refused
  /// unless it set exactly that many rows. [order] and [limit] before it pick
  /// the rows it writes -- the page a `get` with them answers -- and
  /// [returning] answers them as written, the fields named or `*` for every
  /// one: a job queue's claim ([updateReturning]).
  Statement toUpdate(Object patch, {bool all = false, int? require, List<String>? returning}) {
    _assertPlain('update');
    final params = <Object?>[];
    final bind = _binder(params);
    final body = _renderDoc(patch, bind);
    final filter = _requireFilter('update', all, bind);
    return (text: 'set $collection $body$filter${_pick(returning)}${_requireClause(require)}', params: params);
  }

  /// The upsert, `put ... if absent else set {patch}`, not run: a document
  /// whose id or first `@unique` value a row holds sets that row by [patch]
  /// -- an update's, [Computed.inc] and [Computed.expr] with it, and in an
  /// expression `new.f` the document's own `f` -- and every other one is
  /// inserted. The documents' parameters come first, then the patch's.
  Statement toUpsert(Object docs, Object patch, {int? require}) {
    _assertPlain('upsert');
    final list = docs is List ? docs : [docs];
    if (list.isEmpty) throw _refuse('cannot write an empty document list');
    final params = <Object?>[];
    final bind = _binder(params);
    final body = list.map((d) => _renderDoc(d, bind, insert: true)).join(', ');
    final set = _renderDoc(patch, bind);
    final required = _requireClause(require);
    return (
      text: 'put $collection ${list.length == 1 ? body : '[$body]'} if absent else set $set$required',
      params: params,
    );
  }

  /// The `del` of the rows the filter names, not run; with no filter it is
  /// refused unless [all] or a limit bounds it; with [require], refused
  /// unless it deleted exactly that many rows. [order], [limit] and
  /// [returning] as [toUpdate]'s, the rows answered as they were: a pop.
  Statement toDelete({bool all = false, int? require, List<String>? returning}) {
    _assertPlain('delete');
    final params = <Object?>[];
    final filter = _requireFilter('delete', all, _binder(params));
    return (text: 'del $collection$filter${_pick(returning)}${_requireClause(require)}', params: params);
  }

  // ------------------------------------------------------------- running

  Future<Answer> _run(Statement s) {
    final exec = _exec;
    if (exec == null) {
      throw _refuse('query is not bound to a connection: use db.from(...) (toFenecQL() if you only want the text)');
    }
    return exec(s.text, s.params);
  }

  /// Runs the query and hands back its rows -- with what [facet] counted
  /// as their [Rows.facets].
  Future<Rows> rows() async => (await _run(toFenecQL())).rows;

  /// Runs the query and hands back its whole answer: the rows, their
  /// columns, and the facets beside them.
  Future<Answer> answer() => _run(toFenecQL());

  /// The first row with `limit 1`, or null.
  Future<Map<String, Object?>?> first() async {
    final r = await limit(1).rows();
    return r.isEmpty ? null : r.first;
  }

  /// How many rows match: `get ... count`, no row decoded.
  Future<int> count() async {
    final r = await _with(count: true).rows();
    return r.isEmpty ? 0 : r.first['count'] as int;
  }

  /// The path the query took, a line a step; the query runs to tell.
  Future<List<String>> explain() async {
    final s = toFenecQL();
    final r = await _run((text: 'explain ${s.text}', params: s.params));
    return [for (final row in r.rows) row['plan'] as String? ?? ''];
  }

  /// Puts a document -- a map of fields -- or a list of them: how many it
  /// wrote, which with [ifAbsent] leaves out those already held. None is no
  /// statement. [require]: refused ([FenecCode.unmet]) unless it wrote
  /// exactly that many.
  Future<int> insert(Object docs, {bool ifAbsent = false, int? require}) async {
    if (docs is List && docs.isEmpty) return 0;
    return (await _run(toInsert(docs, ifAbsent: ifAbsent, require: require))).affected;
  }

  /// Inserts each document, or, where a row holds its id or `@unique`
  /// value, sets that row by [patch] --
  /// `upsert({'key': k, 'n': 1}, {'n': Computed.expr('n + new.n')})`: the
  /// rows it set and made together. None is no statement.
  Future<int> upsert(Object docs, Object patch, {int? require}) async {
    if (docs is List && docs.isEmpty) return 0;
    return (await _run(toUpsert(docs, patch, require: require))).affected;
  }

  /// Sets the patch's fields on the rows the filter names; with no filter it
  /// is refused unless [all]; with [require], refused ([FenecCode.unmet])
  /// unless it set exactly that many.
  Future<int> update(Object patch, {bool all = false, int? require}) async =>
      (await _run(toUpdate(patch, all: all, require: require))).affected;

  /// Deletes the rows the filter names; with no filter it is refused unless
  /// [all]; with [require], refused ([FenecCode.unmet]) unless it deleted
  /// exactly that many.
  Future<int> delete({bool all = false, int? require}) async =>
      (await _run(toDelete(all: all, require: require))).affected;

  /// [update] that answers the rows it set, as written, under the fields
  /// named -- `*` every one. With [order] and [limit] a job queue's claim:
  /// `from('jobs').where(Cond.raw('run_at <= now()')).order('run_at').limit(10)
  /// .updateReturning({'owner': me, 'run_at': Computed.expr('now() + ?', [30000]),
  /// 'attempts': Computed.inc(1)})`.
  Future<Rows> updateReturning(Object patch,
          {List<String> returning = const ['*'], bool all = false, int? require}) async =>
      (await _run(toUpdate(patch, all: all, require: require, returning: returning))).rows;

  /// [delete] that answers the rows it deleted, as they were: a pop.
  Future<Rows> deleteReturning({List<String> returning = const ['*'], bool all = false, int? require}) async =>
      (await _run(toDelete(all: all, require: require, returning: returning))).rows;
}
