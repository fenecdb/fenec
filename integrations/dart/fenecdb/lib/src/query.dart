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

/// One key of a lookup's order: a field, `asc` or `desc`, and a collation
/// (`tr` or `und`).
class SortKey {
  final String field;
  final String direction;
  final String? collate;
  const SortKey(this.field, [this.direction = 'asc', this.collate]);
}

class Node {
  final String t; // and, or, not, null, in, cmp, raw
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
final _aggregate = RegExp(r'^(count)\(\*?\)$|^(sum|avg|min|max)\(([A-Za-z_][A-Za-z0-9_]*)\)$', caseSensitive: false);

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

/// A select item: a field, or an aggregate spelled as FenecQL spells it --
/// `count(*)`, `sum(total)`, `avg(f)`, `min(f)`, `max(f)` -- answering under
/// that name.
(String, bool) _column(String name) {
  final m = _aggregate.firstMatch(_jsTrim(name));
  if (m == null) return (_pathOf(name), false);
  if (m[1] != null) return ('count(*)', true);
  return ('${m[2]!.toLowerCase()}(${m[3]})', true);
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
    final op = _ops[k] ?? (throw _refuse('unknown operator `$k` (field: $field)'));
    items.add(op == 'in' ? _inCond(field, e.value) : _cmp(field, op, e.value));
  }
  return switch (items.length) {
    0 => throw _refuse('empty condition object (field: $field)'),
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

/// A query over one collection, made by [Fenec.from] or [Query.from].
/// Immutable: each call hands back a new one, so a base query can be kept
/// and branched from.
class Query {
  final String collection;
  final Exec? _exec;
  final List<String>? _project;
  final bool _aggregate;
  final String? _group;
  final List<Node> _cond;
  final _Vector? _near;
  final (String, String)? _match;
  final _Vector? _rerank;
  final (int?, int?)? _fuse;
  final List<_Key> _order;
  final int? _limit;
  final int _offset;
  final bool _count;
  final List<_Level> _lookups;

  Query._(this.collection,
      {Exec? exec,
      List<String>? project,
      bool aggregate = false,
      String? group,
      List<Node> cond = const [],
      _Vector? near,
      (String, String)? match,
      _Vector? rerank,
      (int?, int?)? fuse,
      List<_Key> order = const [],
      int? limit,
      int offset = 0,
      bool count = false,
      List<_Level> lookups = const []})
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
        _count = count,
        _lookups = lookups;

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
    bool? count,
    List<_Level>? lookups,
  }) =>
      Query._(collection,
          exec: identical(exec, _keep) ? _exec : exec as Exec?,
          project: identical(project, _keep) ? _project : project as List<String>?,
          aggregate: aggregate ?? _aggregate,
          group: identical(group, _keep) ? _group : group as String?,
          cond: cond ?? _cond,
          near: identical(near, _keep) ? _near : near as _Vector?,
          match: identical(match, _keep) ? _match : match as (String, String)?,
          rerank: identical(rerank, _keep) ? _rerank : rerank as _Vector?,
          fuse: identical(fuse, _keep) ? _fuse : fuse as (int?, int?)?,
          order: order ?? _order,
          limit: identical(limit, _keep) ? _limit : limit as int?,
          offset: offset ?? _offset,
          count: count ?? _count,
          lookups: lookups ?? _lookups);

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
  /// `select(['status', 'count(*)', 'sum(total)']).group('status')`.
  Query select(List<String> columns) {
    if (columns.isEmpty || columns.contains('*')) return _with(project: null, aggregate: false);
    final cs = columns.map(_column).toList();
    return _with(project: [for (final c in cs) c.$1], aggregate: cs.any((c) => c.$2));
  }

  /// `group field`: a row per value, for a select list that aggregates.
  Query group(String field) => _with(group: _identOf(field));

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
      _with(order: [..._order, _Key(_column(field).$1, _direction(direction), _collation(collate))]);

  /// How many rows come back.
  Query limit(int n) => _with(limit: _whole(n, 'limit'));

  /// How many rows are passed over first.
  Query offset(int n) => _with(offset: _whole(n, 'offset'));

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
    if (_group != null && !_aggregate) throw _refuse("group $_group needs an aggregate in select: 'count(*)'");
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
      final extra = _extraClause();
      if (extra != null) throw _refuse('count cannot be used with `$extra`');
    }

    final sql = StringBuffer('get $collection');
    if (_project != null) sql.write(' select ${_project.join(', ')}');
    final w = _whereOf(_cond, bind);
    if (w != null) sql.write(' where $w');
    if (_group != null) sql.write(' group $_group');
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
    if (_count) sql.write(' count');
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

  String? _extraClause() => _near != null
      ? 'near'
      : _match != null
          ? 'match'
          : _rerank != null
              ? 'rerank'
              : _order.isNotEmpty
                  ? 'order'
                  : _limit != null
                      ? 'limit'
                      : _offset > 0
                          ? 'offset'
                          : _project != null
                              ? 'select'
                              : null;

  // Near, order, limit mean something only to a read; dropped from a write,
  // limit(1).delete() would delete every row.
  void _assertPlain(String verb) {
    final extra = _extraClause();
    if (extra != null) throw _refuse('$verb cannot be used with `$extra`');
    if (_lookups.isNotEmpty) throw _refuse('$verb cannot be used with `lookup`');
    if (verb == 'insert' && _cond.isNotEmpty) throw _refuse('insert cannot be used with `where`');
  }

  // An update or delete of every row is too easy to do by accident and
  // cannot be undone: it has to be asked for, with all.
  String _requireFilter(String verb, bool all, String Function(Object?) bind) {
    final w = _whereOf(_cond, bind);
    if (w != null) return ' where $w';
    if (all) return '';
    throw _refuse('an unfiltered $verb covers the whole collection; if you mean it, $verb({ all: true })');
  }

  static String _renderDoc(Object? doc, String Function(Object?) bind) {
    if (doc is! Map) throw _refuse('expected a document object');
    if (doc.isEmpty) throw _refuse('cannot write an empty document');
    return '{${doc.entries.map((e) => '${_pathOf(e.key.toString())}: ${bind(e.value)}').join(', ')}}';
  }

  /// The `put` of a document -- a map of fields, in its order -- or a list of them, not run.
  Statement toInsert(Object docs) {
    _assertPlain('insert');
    final list = docs is List ? docs : [docs];
    if (list.isEmpty) throw _refuse('cannot write an empty document list');
    final params = <Object?>[];
    final bind = _binder(params);
    final body = list.map((d) => _renderDoc(d, bind)).join(', ');
    return (text: 'put $collection ${list.length == 1 ? body : '[$body]'}', params: params);
  }

  /// The `set` of the rows the filter names, not run; with no filter it is refused unless [all].
  Statement toUpdate(Object patch, {bool all = false}) {
    _assertPlain('update');
    final params = <Object?>[];
    final bind = _binder(params);
    final body = _renderDoc(patch, bind);
    return (text: 'set $collection $body${_requireFilter('update', all, bind)}', params: params);
  }

  /// The `del` of the rows the filter names, not run; with no filter it is refused unless [all].
  Statement toDelete({bool all = false}) {
    _assertPlain('delete');
    final params = <Object?>[];
    return (text: 'del $collection${_requireFilter('delete', all, _binder(params))}', params: params);
  }

  // ------------------------------------------------------------- running

  Future<Answer> _run(Statement s) {
    final exec = _exec;
    if (exec == null) {
      throw _refuse('query is not bound to a connection: use db.from(...) (toFenecQL() if you only want the text)');
    }
    return exec(s.text, s.params);
  }

  /// Runs the query and hands back its rows.
  Future<List<Map<String, Object?>>> rows() async => (await _run(toFenecQL())).rows;

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
  /// wrote. None is no statement.
  Future<int> insert(Object docs) async {
    if (docs is List && docs.isEmpty) return 0;
    return (await _run(toInsert(docs))).affected;
  }

  /// Sets the patch's fields on the rows the filter names; with no filter it is refused unless [all].
  Future<int> update(Object patch, {bool all = false}) async => (await _run(toUpdate(patch, all: all))).affected;

  /// Deletes the rows the filter names; with no filter it is refused unless [all].
  Future<int> delete({bool all = false}) async => (await _run(toDelete(all: all))).affected;
}
