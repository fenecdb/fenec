# The native library for macOS: the dynamic XCFramework's macOS slice,
# FenecFFI.framework (prepare.sh copies it into Frameworks/), embedded by
# CocoaPods and opened by name from Dart, as on iOS.
Pod::Spec.new do |s|
  s.name             = 'fenecdb_flutter'
  s.version          = '0.1.7'
  s.summary          = 'fenecdb in a Flutter app: the native library for macOS.'
  s.homepage         = 'https://github.com/fenecdb/fenec'
  s.license          = { :type => 'Apache-2.0' }
  s.author           = { 'fenecdb' => 'https://github.com/fenecdb' }
  s.source           = { :path => '.' }
  s.platform         = :osx, '12.0'
  s.vendored_frameworks = 'Frameworks/FenecFFI.xcframework'
  s.dependency 'FlutterMacOS'
end
