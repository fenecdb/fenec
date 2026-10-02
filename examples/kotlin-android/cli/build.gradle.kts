plugins {
    kotlin("jvm")
    application
}

val fenecVersion: String by project

kotlin { compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_11) } }
java {
    sourceCompatibility = JavaVersion.VERSION_11
    targetCompatibility = JavaVersion.VERSION_11
}

dependencies {
    implementation(project(":notes"))
    // The library for the JVM; the native library comes from FENEC_LIBRARY (README).
    implementation("com.fenecdb:fenecdb:$fenecVersion")
}

application {
    mainClass.set("notes.cli.MainKt")
}

tasks.named<JavaExec>("run") {
    // Gradle runs a program in the project's directory; notes.fenec goes
    // where `gradle` was started instead.
    workingDir = gradle.startParameter.currentDir
    System.getenv("FENEC_LIBRARY")?.let { systemProperty("fenec.library", it) }
    standardInput = System.`in`
}
