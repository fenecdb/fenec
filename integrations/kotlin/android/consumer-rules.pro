# The JNI functions are found by their class's and methods' names: R8 must
# keep both, or the app's release build calls into names the library does
# not export.
-keep class io.github.fenecdb.FenecNative { *; }
