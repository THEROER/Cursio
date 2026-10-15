plugins {
    id("com.android.application")
}

// `cargo xtask android` stages the Rust bridge here under its ABI directory.
// Pointing at it rather than copying keeps one library on disk: an APK built
// after a Rust rebuild packages that rebuild, never a stale copy.
val rustJniLibs = rootDir.resolve("../../../target/android/jniLibs")

android {
    namespace = "dev.cursio.mesh"
    compileSdk = 36

    defaultConfig {
        applicationId = "dev.cursio.mesh"
        // The Rust audio backend opens AAudio streams with input presets
        // (the echo canceller) and usage attributes, both API 28.
        minSdk = 28
        targetSdk = 36
        versionCode = 1
        versionName = "0.1.0"

        // The Rust core is built for arm64 only; an APK claiming other ABIs
        // would install on a device that then fails at `loadLibrary`.
        ndk {
            abiFilters += "arm64-v8a"
        }
    }

    sourceSets {
        named("main") {
            jniLibs.srcDir(rustJniLibs)
        }
    }

    buildTypes {
        // Personal mesh software installed over adb, not a store listing: the
        // debug key lets a release build install without a keystore to manage.
        release {
            isMinifyEnabled = false
            signingConfig = signingConfigs.getByName("debug")
        }
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    packaging {
        jniLibs {
            // The bridge is loaded once, at startup; extracting it to disk
            // costs nothing that matters and keeps `adb shell` debugging of
            // the library straightforward.
            useLegacyPackaging = true
        }
    }
}

dependencies {
    implementation("androidx.core:core:1.13.1")
}
