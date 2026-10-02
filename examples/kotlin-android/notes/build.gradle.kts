plugins {
    kotlin("jvm")
}

val fenecVersion: String by project

// Java 11 bytecode, which Android takes, from whichever JDK runs Gradle.
kotlin { compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_11) } }
java {
    sourceCompatibility = JavaVersion.VERSION_11
    targetCompatibility = JavaVersion.VERSION_11
}

dependencies {
    // compileOnly: the CLI brings the JVM library and the app the AAR, which
    // holds the same classes -- both here would put them in the app twice.
    compileOnly("com.fenecdb:fenecdb:$fenecVersion")
}
