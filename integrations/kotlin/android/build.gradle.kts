// The AAR: the fenecdb library's Kotlin with the native library for each
// ABI (`build/jniLibs/<abi>/libfenec_ffi.so`, which build-aar.sh fills with
// the NDK). Configured only where an Android SDK is installed
// (settings.gradle.kts).
plugins {
    id("com.android.library")
    kotlin("android")
    `maven-publish`
    signing
}

android {
    namespace = "io.github.fenecdb"
    compileSdk = 34

    defaultConfig {
        // Kotlin's coroutines want 21, and so does the NDK's lowest
        // supported level for 64-bit ABIs.
        minSdk = 21
        consumerProguardFiles("consumer-rules.pro")
    }

    sourceSets["main"].apply {
        // One source of the library: the JVM module's.
        java.srcDir("../fenecdb/src/main/kotlin")
        jniLibs.srcDir("build/jniLibs")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
    }

    publishing {
        singleVariant("release") {
            withSourcesJar()
        }
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_11)
    }
}

dependencies {
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.1")
}

publishing {
    publications {
        create<MavenPublication>("android") {
            artifactId = "fenecdb-android"
            afterEvaluate { from(components["release"]) }
            pom {
                name.set("fenecdb-android")
                description.set("fenecdb embedded in an Android app: a file on the device, live queries as a Flow")
                url.set("https://github.com/fenecdb/fenec")
                licenses { license { name.set("Apache-2.0"); url.set("https://www.apache.org/licenses/LICENSE-2.0") } }
                developers { developer { id.set("fenecdb"); name.set("fenecdb") } }
                scm { url.set("https://github.com/fenecdb/fenec") }
            }
        }
    }
}

signing {
    val key = System.getenv("SIGNING_KEY")
    if (key != null) {
        useInMemoryPgpKeys(key, System.getenv("SIGNING_PASSWORD"))
        sign(publishing.publications)
    }
}
