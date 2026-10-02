/// fenecdb embedded in a Dart or Flutter app: a vector-native database in a
/// file on the device, through the native library (crates/fenec-ffi) --
/// FenecQL or the query builder, and live queries as Streams.
library;

export 'src/fenec.dart' show Fenec, FenecException, FenecCode, Answer, Changes;
export 'src/live.dart' show LiveQueries;
export 'src/query.dart' show Query, Cond, SortKey, Statement;
