# EntroTunnel desktop app (Tauri)

Clash-Verge-style GUI over the shared `entrotunnel-client` engine. It is a
separate Cargo workspace (see `src-tauri/Cargo.toml`'s empty `[workspace]`),
path-depending on the engine crates.

## Develop

This app uses **Yarn** (classic).

```bash
yarn install

# Icons are required by the bundler — generate them once from any square PNG:
yarn tauri icon path/to/logo.png     # writes src-tauri/icons/*

yarn tauri dev
```

> Global-proxy / VPN modes create a TUN device and need elevated privileges.
> On macOS/Linux run the dev build from an elevated shell, or wire a privileged
> helper for production. The CLI (`entrotunnel-cli`) is the easiest way to test
> the tunnel itself.

## Structure

- `src/` — React + TS frontend (`App.tsx` holds the layout + pages, `api.ts`
  wraps the Tauri commands). Pages: **Home** (pick profile/server + mode, connect,
  latency), **Profiles** (server-only editor, import/export), **Settings** (mode
  parameters + split-tunnel routes), **Logs**.
- `src-tauri/src/main.rs` — command layer: `list_profiles`, `upsert_profile`,
  `remove_profile`, `import_profile`, `export_profile`, `get_state`,
  `set_settings`, `set_active_profile`, `connect_profile`, `disconnect`,
  `status`, `ping_server`, `gen_psk`. Each drives `entrotunnel_client::Engine`.
- A **profile** is server config only (`name`, `servers`, `selected_server`); the
  mode and its parameters live in device-local **settings**. They are composed
  into the engine config at connect time.
- Persistence (app config dir): `profiles.json` (the profiles) and
  `settings.json` (mode + TUN/HTTP/routes + active profile).
