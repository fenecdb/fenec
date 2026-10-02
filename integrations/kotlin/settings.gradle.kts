// fenecdb for Kotlin and Java: the library (`fenecdb`, Kotlin on the JVM,
// which Android takes as it is) and, where an Android SDK is installed, the
// AAR that carries it with the native library for each ABI (`android`).
// JVM tests need neither the SDK nor the NDK: `make kotlin-test` runs them
// in a gradle:8-jdk17 container against the library built for Linux.
pluginManagement {
    repositories {
        gradlePluginPortal()
        google()
        mavenCentral()
    }
}

dependencyResolutionManagement {
    repositories {
        google()
        mavenCentral()
    }
}

rootProject.name = "fenecdb-kotlin"
include("fenecdb")

val sdk = System.getenv("ANDROID_HOME") ?: System.getenv("ANDROID_SDK_ROOT")
if (sdk != null && file(sdk).isDirectory) include("android")
