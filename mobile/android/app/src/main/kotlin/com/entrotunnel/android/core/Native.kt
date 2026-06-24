package com.entrotunnel.android.core

/**
 * The JNI bridge to the Rust core (`libentrotunnel_jni.so`, built from
 * `mobile/android/rust`). All tunneling — handshake, chain proxy, encryption,
 * packet bridging — happens in Rust; Kotlin only supplies config + the
 * VpnService fd and reads back status/logs.
 *
 * Two-phase connect (the VpnService.Builder needs the server-assigned IP before
 * it can establish the TUN):
 *   1. [nativeConnect] — connect + handshake, returns the network-config JSON.
 *   2. [nativeStartBridge] — hand it the established fd to start the packet loop.
 * HTTP-proxy mode skips step 2 (no VpnService): [nativeConnect] runs the local
 * proxy directly.
 */
object Native {
    init {
        System.loadLibrary("entrotunnel_jni")
    }

    /** Returns network-config JSON (`mode`,`assigned_ip`,…) or `{"error":"…"}`. */
    external fun nativeConnect(profileJson: String, settingsJson: String): String

    /** Packet modes only: give the engine the VpnService fd. "" on success. */
    external fun nativeStartBridge(tunFd: Int): String

    external fun nativeStop()

    /** JSON: `{running, assigned_ip, peers:[{ip,name}], error}`. */
    external fun nativeStatus(): String

    /** Newline-joined recent log lines (engine + core). */
    external fun nativeLogs(): String
}
