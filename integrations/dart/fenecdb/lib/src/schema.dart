part of 'fenec.dart';

/// A difference no open applies -- one that would lose data or could mean
/// two things -- and how to resolve it, as the engine writes them.
class SchemaRefusal {
  final String kind;
  final String collection;

  /// The field, or the path into a json field; null for the collection.
  final String? field;
  final String message;
  final String fix;

  SchemaRefusal._(Map<String, Object?> v)
      : kind = v['kind'] as String,
        collection = v['collection'] as String,
        field = v['field'] as String?,
        message = v['message'] as String,
        fix = v['fix'] as String;
}

/// What the engine found comparing the database with the schema the app
/// declares, and did: the FenecQL that adds what is missing, the
/// migrations it recorded, and what it refused.
class SchemaPlan {
  final bool applied;

  /// Whether the migrations recorded ran: a database made from the schema
  /// records them without running them.
  final bool ran;
  final List<int> migrations;
  final List<String> statements;
  final List<SchemaRefusal> refusals;

  SchemaPlan._(Map<String, Object?> v)
      : applied = v['applied'] as bool,
        ran = v['ran'] as bool,
        migrations = (v['migrations'] as List).cast<int>(),
        statements = (v['statements'] as List).cast<String>(),
        refusals = [
          for (final r in v['refusals'] as List) SchemaRefusal._(r as Map<String, Object?>),
        ];
}

/// A schema the database differs from in what no open applies: [refusals]
/// says each difference.
class SchemaException implements Exception {
  final List<SchemaRefusal> refusals;
  SchemaException(this.refusals);

  @override
  String toString() =>
      "SchemaException: the database's schema differs from the app's:${refusals.map((r) => '\n  - ${r.message}\n    ${r.fix}').join()}";
}

/// A schema declared as FenecQL, checked and applied in the engine.
extension FenecSchema on Fenec {
  /// Brings the database to [fenecql] -- `create collection` and `create
  /// index` statements, a `schema.fenecql` file -- as every SDK does, in
  /// the engine: the [migrations] not yet recorded run first, in order, and
  /// what only adds is made, all one block. Anything that would lose data
  /// or could mean two things throws a [SchemaException] naming each
  /// difference, and nothing is written. With `apply: false`, what an apply
  /// would do.
  Future<SchemaPlan> schema(
    String fenecql, {
    List<String> migrations = const [],
    bool apply = true,
  }) async {
    if (_closed) throw FenecException(FenecCode.misuse, 'the database was closed');
    final request = jsonEncode({
      'format': 1,
      'fenecql': fenecql,
      'migrations': migrations,
    });
    final out = Fenec._answer(
      await _worker.call('schema', [_handle, request, apply ? 1 : 0]),
    );
    lives.touch();
    final plan = SchemaPlan._(jsonDecode(out) as Map<String, Object?>);
    if (plan.refusals.isNotEmpty) throw SchemaException(plan.refusals);
    return plan;
  }
}
