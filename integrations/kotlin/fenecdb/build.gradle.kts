plugins {
    kotlin("jvm")
    `java-library`
    `maven-publish`
    signing
}

kotlin {
    jvmToolchain(17)
    compilerOptions {
        // Android takes Java 11 bytecode with no desugaring of its own.
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_11)
    }
    // The rows `fenec types --lang kotlin` writes for
    // integrations/types-golden's schema: compiled with the tests, which
    // read one off a row.
    sourceSets["test"].kotlin.srcDir("../../types-golden")
}

java {
    targetCompatibility = JavaVersion.VERSION_11
    withSourcesJar()
    withJavadocJar()
}

dependencies {
    // Coroutines for the suspending calls and the Flow a live query is; no
    // JSON library: the answers are read by a reader of the library's own,
    // as org.json is Android's alone and kotlinx.serialization would bring
    // a compiler plugin to every app for what a page of code does.
    api("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.1")
    testImplementation(kotlin("test"))
    testImplementation("org.junit.jupiter:junit-jupiter:5.10.3")
    testImplementation("org.jetbrains.kotlinx:kotlinx-coroutines-test:1.8.1")
}

tasks.test {
    useJUnitPlatform()
    // The native library, the golden file and the fenec-server the sync
    // tests start, as `make kotlin-test` passes them.
    System.getenv("FENEC_LIBRARY")?.let { systemProperty("fenec.library", it) }
    System.getenv("FENEC_GOLDEN")?.let { systemProperty("fenec.golden", it) }
    System.getenv("FENEC_SERVER")?.let { systemProperty("fenec.server", it) }
    // A failure named in the log, with its assertion and stack: the report
    // is an HTML file on the runner. run-tests.sh runs Gradle with `-q`,
    // which logs at the quiet level, and that level's settings are its
    // own -- set for the lifecycle level alone, a CI log said "1 failed"
    // and nothing of which.
    testLogging {
        val failures: org.gradle.api.tasks.testing.logging.TestLogging.() -> Unit = {
            events("failed")
            exceptionFormat = org.gradle.api.tasks.testing.logging.TestExceptionFormat.FULL
            showExceptions = true
            showCauses = true
            showStackTraces = true
        }
        failures()
        quiet(failures)
    }
}

publishing {
    // A directory laid out as a Maven repository, signatures and checksums
    // beside each file: packages.yml zips it into the bundle Maven
    // Central's Portal takes, with no publishing plugin.
    repositories {
        maven {
            name = "local"
            url = uri(rootProject.layout.buildDirectory.dir("repo"))
        }
    }
    publications {
        create<MavenPublication>("jvm") {
            artifactId = "fenecdb"
            from(components["java"])
            pom {
                name.set("fenecdb")
                description.set("fenecdb embedded in a JVM or Android app: a file on the device, live queries as a Flow")
                url.set("https://github.com/fenecdb/fenec")
                licenses { license { name.set("Apache-2.0"); url.set("https://www.apache.org/licenses/LICENSE-2.0") } }
                developers { developer { id.set("fenecdb"); name.set("fenecdb") } }
                scm { url.set("https://github.com/fenecdb/fenec") }
            }
        }
    }
}

// Signed only where a key is given (the release's job): a local build and
// the tests need none.
signing {
    val key = System.getenv("SIGNING_KEY")
    if (key != null) {
        useInMemoryPgpKeys(key, System.getenv("SIGNING_PASSWORD"))
        sign(publishing.publications)
    }
}
