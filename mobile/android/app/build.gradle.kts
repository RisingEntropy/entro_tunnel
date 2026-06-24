plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
    id("org.jetbrains.kotlin.plugin.serialization")
    id("org.mozilla.rust-android-gradle.rust-android")
}

android {
    namespace = "com.entrotunnel.android"
    compileSdk = 35
    // Pin to the NDK actually installed in the SDK. Without this, AGP looks for
    // its built-in default NDK version and fails with "NDK is not installed"
    // even though a (differently-versioned) NDK is present. Update this string
    // if you install a different NDK (SDK Manager → SDK Tools → NDK).
    ndkVersion = "30.0.14904198"

    defaultConfig {
        applicationId = "com.entrotunnel.android"
        minSdk = 24
        targetSdk = 35
        versionCode = 1
        versionName = "0.1"
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
    }

    buildTypes {
        release {
            isMinifyEnabled = false
            proguardFiles(getDefaultProguardFile("proguard-android-optimize.txt"), "proguard-rules.pro")
        }
    }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    buildFeatures { compose = true }
}

// Build the Rust native core (../rust → libentrotunnel_jni.so) for each ABI.
cargo {
    module = "../rust"
    libname = "entrotunnel_jni"
    // Plugin ABI names: "arm64"=arm64-v8a, "arm"=armeabi-v7a, plus x86_64.
    targets = listOf("arm64", "arm", "x86_64")
    profile = "release"

    // Android Studio launched from Finder/Dock does NOT inherit your shell PATH,
    // so the plugin can't find rustup's `cargo`/`rustc` (they live in
    // ~/.cargo/bin) and fails with: Cannot run program "rustc" (No such file).
    // Point at them by absolute path. Derived from user.home so it stays
    // portable across machines/users (assumes a default rustup install).
    val cargoBin = "${System.getProperty("user.home")}/.cargo/bin"
    cargoCommand = "$cargoBin/cargo"
    rustcCommand = "$cargoBin/rustc"

    // The plugin links via a generated Python wrapper (linker-wrapper.py, which
    // rewrites -lgcc→-lunwind for NDK≥23). It invokes it as bare `python`, which
    // doesn't exist on macOS (only `python3`) and isn't on the GUI PATH anyway —
    // so point at python3 directly, or linking fails after compilation.
    pythonCommand = "/usr/bin/python3"
}

// Compile the .so before the APK is assembled (and before jniLibs are merged).
tasks.named("preBuild").configure { dependsOn("cargoBuild") }

dependencies {
    implementation("androidx.core:core-ktx:1.13.1")
    implementation("androidx.activity:activity-compose:1.9.2")
    implementation(platform("androidx.compose:compose-bom:2024.09.02"))
    implementation("androidx.compose.ui:ui")
    implementation("androidx.compose.ui:ui-tooling-preview")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.material:material-icons-extended")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.6")
    implementation("androidx.lifecycle:lifecycle-viewmodel-compose:2.8.6")
    implementation("org.jetbrains.kotlinx:kotlinx-serialization-json:1.7.1")
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-android:1.8.1")
    debugImplementation("androidx.compose.ui:ui-tooling")
}
