// The phone's half of the Cursio app that only Kotlin can do: the foreground
// service that owns the node, the boot receiver, and the permissions. Not a
// project of its own: `app/build.rs` points tauri-build at this directory and
// the generated Android project includes it as a library module, so its
// manifest merges into the app's.
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}

android {
    namespace = "dev.cursio.mesh"
    compileSdk = 36

    defaultConfig {
        // The Rust audio backend opens AAudio streams with input presets (the
        // echo canceller) and usage attributes, both API 28.
        minSdk = 28
        consumerProguardFiles("consumer-rules.pro")
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_11
        targetCompatibility = JavaVersion.VERSION_11
    }
}

kotlin {
    compilerOptions {
        jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_11)
    }
}

dependencies {
    // Provided by the app, which always includes Tauri's own module.
    compileOnly(project(":tauri-android"))

    // `ServiceCompat`, and the `ActivityResult` a plugin's activity callback
    // receives. The app already ships both.
    implementation("androidx.core:core:1.13.1")
    implementation("androidx.activity:activity:1.10.1")
}
