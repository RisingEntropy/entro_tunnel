# EntroTunnel — Android

The Android client, modeled after Clash for Android. The **core is the existing
Rust engine** (`core/core` + `core/client-core`, unchanged): a thin JNI
crate (`rust/`) reuses it and adds only the Android glue (the VpnService-fd
packet loop + JNI). All tunneling — handshake, **multi-hop proxy chain**,
encryption, packet bridging — runs in Rust.

## Architecture

```
┌─────────────── Kotlin (Jetpack Compose) ───────────────┐
│  MainActivity / AppRoot   Home · Profiles · Logs        │
│  EngineController         status polling, start/stop    │
│  EntroVpnService          system VPN → TUN fd            │
│  core/Native (JNI)        external fun nativeConnect…    │
└───────────────────────────┬─────────────────────────────┘
                            │ JNI  (libentrotunnel_jni.so)
┌───────────────────────────▼─────────────────────────────┐
│  rust/  (entrotunnel-jni, cdylib)                        │
│    connect_chain() ─ reuses core/client-core           │
│    fd_bridge()     ─ VpnService fd ⇆ Frame::Packet       │
└──────────────────────────────────────────────────────────┘
```

**Global mode uses the *system VPN* (`VpnService`)** — we do *not* create a raw
NIC (impossible without root). The OS hands us a TUN fd after the user approves
the consent dialog; Rust reads/writes packets on it. Routing/DNS/split-tunnel
are set on `VpnService.Builder`, so Rust runs **no** `ip`/`route` commands.
Because the handshake assigns the virtual IP, connecting is two-phase:
`nativeConnect` (handshake → network config) → build the VPN → `nativeStartBridge(fd)`.
HTTP-proxy mode uses **no** VpnService (a local proxy apps point at).

## Build (in Android Studio — recommended)

Prereqs:
1. **Android Studio** (Giraffe+), with the **NDK** and CMake (SDK Manager →
   SDK Tools → "NDK (Side by side)").
2. **Rust + Android targets**:
   ```
   rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
   ```
3. Open `mobile/android/` in Android Studio. Set `ANDROID_NDK_HOME` (or it is
   picked up from the SDK). Run the `app` configuration on a device/emulator.

The `org.mozilla.rust-android-gradle` plugin compiles `rust/` for each ABI and
drops `libentrotunnel_jni.so` into the APK (`cargoBuild` runs before `preBuild`).

### CLI build
```
cd mobile/android
./gradlew assembleDebug        # needs gradlew (Android Studio generates it, or run `gradle wrapper`)
```

### Validate just the Rust core (no NDK needed)
```
cd mobile/android/rust
cargo check --target aarch64-linux-android --no-default-features   # tcp-only, pure Rust
```

## Status — what works vs. needs a device pass

This is the **initial implementation**; it compiles the Rust core (verified) and
is wired end-to-end, but the Kotlin/VPN paths have **not been run on a device**
yet — expect to iterate in Android Studio. Built:

- ✅ Rust native core (JNI + chain proxy + VpnService-fd bridge) — compiles for
  `aarch64-linux-android`.
- ✅ Compose UI: Home (connect, profile/mode/**chain**, VPN peers), Profiles
  (import `entro://`, delete), Logs.
- ✅ `VpnService` with the two-phase connect, self-exclusion, global vs VPN-LAN
  routing, foreground notification.

To do / verify on device:

- Full **profile editor** + TOML import/export (v1 imports `entro://` links only;
  TOML import would reuse the Rust parser via a new JNI call).
- **Per-app split tunnel** (the manifest has `QUERY_ALL_PACKAGES`; wire
  `addAllowed/DisallowedApplication` from a settings screen).
- **Settings** screen (tun params, split-tunnel routes, requested IP).
- Traffic stats, reconnect-on-network-change, tile/quick-settings.
- `quic` transport on Android (currently the JNI build enables tcp/tls/ws; add
  `quic` once verified — note QUIC can only be a chain's first hop).
- The launcher icon is a placeholder — replace with the wall-breach logo.

## Notes

- The app **excludes itself** from the VPN (`addDisallowedApplication`) so the
  engine's socket to the server bypasses the tunnel (no per-socket `protect()`
  needed). Multi-hop chains and WS/TLS-over-relay work the same as on desktop.
- Min SDK 24, target 35. ABIs: arm64-v8a, armeabi-v7a, x86_64.
