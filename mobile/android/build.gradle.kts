plugins {
    id("com.android.application") version "8.5.2" apply false
    id("org.jetbrains.kotlin.android") version "2.0.20" apply false
    id("org.jetbrains.kotlin.plugin.compose") version "2.0.20" apply false
    id("org.jetbrains.kotlin.plugin.serialization") version "2.0.20" apply false
    // Builds the Rust `cdylib` (../rust) for the configured ABIs and drops the
    // .so into the APK's jniLibs. Needs the Android NDK + Rust android targets.
    id("org.mozilla.rust-android-gradle.rust-android") version "0.9.6" apply false
}
