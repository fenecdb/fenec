# The native library for iOS: the XCFramework integrations/swift builds
# (prepare.sh copies it into Frameworks/), a static library a slice, linked
# into the app. The Dart side finds its functions in the process, by name,
# where nothing in the app calls them -- so the linker is told to keep the
# whole archive rather than strip what it sees no caller of.
Pod::Spec.new do |s|
  s.name             = 'fenecdb_flutter'
  s.version          = '0.1.7'
  s.summary          = 'fenecdb in a Flutter app: the native library for iOS.'
  s.homepage         = 'https://github.com/fenecdb/fenec'
  s.license          = { :type => 'Apache-2.0' }
  s.author           = { 'fenecdb' => 'https://github.com/fenecdb' }
  s.source           = { :path => '.' }
  s.platform         = :ios, '15.0'
  s.vendored_frameworks = 'Frameworks/FenecFFI.xcframework'
  s.user_target_xcconfig = {
    'OTHER_LDFLAGS' => '-force_load "${PODS_XCFRAMEWORKS_BUILD_DIR}/fenecdb_flutter/libfenec_ffi.a"'
  }
  s.dependency 'Flutter'
end
