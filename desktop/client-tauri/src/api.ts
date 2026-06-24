// Thin wrappers over the Rust (Tauri) command layer. The GUI never tunnels by
// itself — every call drives the shared `entrotunnel-client` engine, the same
// core the CLI uses.
//
// Data model (mirrors core/client-core/src/config.rs):
//   * Profile            — *server config only* (endpoints/tokens/crypto).
//   * ConnectionSettings — device-local: which mode to run + its parameters.
// They are composed into the engine config at connect time.
import { invoke } from "@tauri-apps/api/core";

export type Transport = "tcp" | "ws" | "quic";
// Ordered loosely by "intensity": TUN captures everything, system proxy flips the
// OS proxy switch, HTTP proxy is opt-in per app, VPN is the virtual LAN.
export type Mode = "global_proxy" | "system_proxy" | "http_proxy" | "vpn";

// Split-tunnel rule: send `target` (domain or IP/CIDR) out `via`
// ("tunnel" | "direct" | a NIC name) instead of the default for the mode.
export interface RouteRule {
  target: string;
  via: string;
  gateway?: string | null;
}

// One server endpoint within a profile.
export interface ServerEntry {
  name: string;
  host: string;
  port: number;
  transport: Transport;
  token: string;
  noise_psk: string;
  tls_skip_verify?: boolean;
  server_name?: string | null;
}

// A profile is server config only — no mode, no TUN settings.
export interface Profile {
  name: string;
  selected_server?: string | null;
  servers: ServerEntry[];
}

// Whitelist vs blacklist for the global-proxy catch-all:
//  - blacklist: tunnel everything; rules carve out direct exceptions (default).
//  - whitelist: tunnel ONLY the listed rules; everything else stays direct.
export type SplitMode = "blacklist" | "whitelist";

// Device-local connection settings (the "local connection" choices).
export interface ConnectionSettings {
  mode: Mode;
  requested_ip?: string | null;
  client_name?: string | null;
  tun_name: string;
  http_listen: string;
  // Also join the server's VPN peer LAN regardless of mode (reach other devices
  // by virtual IP). Implied true in "vpn" mode; in a proxy mode it adds a LAN TUN
  // (needs admin/root). See ClientConfig::join_vpn in the Rust core.
  join_vpn?: boolean;
  routes?: RouteRule[];
  split_mode?: SplitMode;
  // Proxy chain: ordered server names (from the active profile) to relay through.
  // Empty / <2 hops = a normal single-server connection. The egress mode runs at
  // the last hop; when joining the VPN, the LAN is the first hop.
  chain?: string[];
  // IPv6 kill-switch: in global-proxy mode, when the server is v4-only, block the
  // host's native IPv6 so it can't bypass the tunnel and leak the real IP. On by
  // default. (undefined is treated as true by the Rust core.)
  ipv6_killswitch?: boolean;
}

export interface LocalState {
  settings: ConnectionSettings;
  active_profile?: string | null;
}

// The active server of a profile (selected, else first).
export function activeServer(p?: Profile): ServerEntry | undefined {
  if (!p || !p.servers || p.servers.length === 0) return undefined;
  return p.servers.find((s) => s.name === p.selected_server) ?? p.servers[0];
}

// One VPN peer on the same server (virtual IP + friendly name).
export interface Peer {
  ip: string;
  name: string;
}

export interface Status {
  connected: boolean;
  profile?: string | null;
  assigned_ip?: string | null;
  mode?: Mode | null;
  error?: string | null;
  // Other VPN members on this server (only when connected as a VPN member).
  peers?: Peer[];
}

export const MODE_LABEL: Record<Mode, string> = {
  global_proxy: "Global proxy (TUN)",
  system_proxy: "System proxy",
  http_proxy: "HTTP proxy",
  vpn: "VPN (virtual LAN)",
};

export const MODE_HINT: Record<Mode, string> = {
  global_proxy: "All traffic through the server via a virtual NIC (TUN). Needs admin/root.",
  system_proxy: "Sets the OS system proxy to the local listener — proxy-aware apps use the tunnel, others are left alone.",
  http_proxy: "Only apps you point at the local HTTP proxy are tunnelled.",
  vpn: "Reach other devices connected to the same server by their virtual IP.",
};

export const api = {
  listProfiles: () => invoke<Profile[]>("list_profiles"),
  saveProfile: (profile: Profile) => invoke<void>("upsert_profile", { profile }),
  removeProfile: (name: string) => invoke<void>("remove_profile", { name }),
  importProfile: (link: string) => invoke<Profile>("import_profile", { link }),
  exportProfile: (name: string) => invoke<string>("export_profile", { name }),
  // Export the full config (servers + current connection settings) as a
  // CLI-ready client.toml; returns the saved path and its contents.
  exportProfileToml: (name: string) =>
    invoke<{ path: string; toml: string }>("export_profile_toml", { name }),
  // Export ALL profiles to one TOML bundle file; returns its path + contents.
  exportAllProfiles: () =>
    invoke<{ path: string; toml: string }>("export_all_profiles"),
  // Import every profile from a TOML bundle; returns how many were imported.
  importProfilesToml: (toml: string) => invoke<number>("import_profiles_toml", { toml }),

  getState: () => invoke<LocalState>("get_state"),
  setSettings: (settings: ConnectionSettings) => invoke<void>("set_settings", { settings }),
  setActiveProfile: (name: string | null) => invoke<void>("set_active_profile", { name }),

  connect: (name: string) => invoke<void>("connect_profile", { name }),
  disconnect: () => invoke<void>("disconnect"),
  status: () => invoke<Status>("status"),
  getLogs: () => invoke<string[]>("get_logs"),
  genPsk: () => invoke<string>("gen_psk"),
  // Privilege handling for TUN modes (global proxy / VPN need root/admin).
  isElevated: () => invoke<boolean>("is_elevated"),
  relaunchElevated: () => invoke<void>("relaunch_elevated"),
  pingServer: (profile: string, server: string) =>
    invoke<number>("ping_server", { profile, server }),
};
