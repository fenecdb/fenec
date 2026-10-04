# The native library for iOS: FenecFFI.framework, a dynamic framework for
# the device and the simulator, which integrations/swift/build-xcframework.sh
# --dynamic makes and prepare.sh copies into Frameworks/. CocoaPods embeds a
# vendored dynamic framework in the app, and the Dart side opens it by name
# (`FenecFFI.framework/FenecFFI`). A static library kept whole with
# -force_load had the Runner link a file the pod's "Copy XCFrameworks" phase
# makes with no order declared against it, and `flutter build ios` stopped
# at "Build input file cannot be found".
Pod::Spec.new do |s|
  s.name             = 'fenecdb_flutter'
  s.version          = '0.1.10'
  s.summary          = 'fenecdb in a Flutter app: the native library for iOS.'
  s.homepage         = 'https://github.com/fenecdb/fenec'
  s.license          = { :type => 'Apache-2.0' }
  s.author           = { 'fenecdb' => 'https://github.com/fenecdb' }
  s.source           = { :path => '.' }
  s.platform         = :ios, '15.0'
  s.vendored_frameworks = 'Frameworks/FenecFFI.xcframework'
  s.dependency 'Flutter'
end
