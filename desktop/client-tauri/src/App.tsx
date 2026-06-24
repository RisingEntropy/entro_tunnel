import { useEffect, useState, useCallback, useRef } from "react";
import logoUrl from "./logo.svg";
import {
  api,
  Profile,
  ServerEntry,
  Status,
  Mode,
  Transport,
  ConnectionSettings,
  LocalState,
  RouteRule,
  activeServer,
  MODE_LABEL,
  MODE_HINT,
} from "./api";

type Page = "home" | "profiles" | "settings" | "logs";

const NAV: { id: Page; ico: string; label: string }[] = [
  { id: "home", ico: "⌂", label: "Home" },
  { id: "profiles", ico: "≣", label: "Profiles" },
  { id: "settings", ico: "⚙", label: "Settings" },
  { id: "logs", ico: "▤", label: "Logs" },
];

const MODES: Mode[] = ["global_proxy", "system_proxy", "http_proxy", "vpn"];

const DEFAULT_STATE: LocalState = {
  settings: { mode: "global_proxy", tun_name: "et0", http_listen: "127.0.0.1:7890", routes: [], split_mode: "blacklist", ipv6_killswitch: true },
  active_profile: null,
};

export default function App() {
  const [page, setPage] = useState<Page>("home");
  const [profiles, setProfiles] = useState<Profile[]>([]);
  const [status, setStatus] = useState<Status>({ connected: false });
  const [local, setLocal] = useState<LocalState>(DEFAULT_STATE);
  // null = unknown (running outside Tauri); false = needs elevation for TUN modes.
  const [elevated, setElevated] = useState<boolean | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [p, s, st, el] = await Promise.all([
        api.listProfiles(),
        api.status(),
        api.getState(),
        api.isElevated().catch(() => null),
      ]);
      setProfiles(p);
      setStatus(s);
      setLocal(st ?? DEFAULT_STATE);
      setElevated(el);
    } catch {
      /* running outside Tauri (plain vite) — ignore */
    }
  }, []);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 2000);
    return () => clearInterval(t);
  }, [refresh]);

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="brand">
          <img className="logo" src={logoUrl} alt="EntroTunnel" />
          <h1>EntroTunnel</h1>
        </div>
        <nav className="nav">
          {NAV.map((n) => (
            <button key={n.id} className={page === n.id ? "active" : ""} onClick={() => setPage(n.id)}>
              <span className="ico">{n.ico}</span>
              {n.label}
            </button>
          ))}
        </nav>
        <div className="spacer" />
        <div className={"status-chip" + (status.connected ? " on" : "")}>
          <span className="d" />
          {status.connected ? `Connected · ${status.assigned_ip ?? ""}` : "Disconnected"}
        </div>
      </aside>

      <main className="content">
        {page === "home" && (
          <Home status={status} profiles={profiles} local={local} elevated={elevated} onChange={refresh} />
        )}
        {page === "profiles" && <Profiles profiles={profiles} onChange={refresh} />}
        {page === "settings" && <SettingsPage local={local} onChange={refresh} />}
        {page === "logs" && <Logs />}
      </main>
    </div>
  );
}

/* ============================== Home ============================== */

function Home({
  status,
  profiles,
  local,
  elevated,
  onChange,
}: {
  status: Status;
  profiles: Profile[];
  local: LocalState;
  elevated: boolean | null;
  onChange: () => void;
}) {
  const [latency, setLatency] = useState<Record<string, number | string>>({});
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [askElevate, setAskElevate] = useState(false);
  const connected = status.connected;
  // Fall back to the first profile when active_profile is unset OR points at a
  // profile that no longer exists (e.g. it was renamed/deleted) — otherwise the
  // page wrongly shows "No profile" even though profiles are present.
  const activeName =
    local.active_profile && profiles.some((p) => p.name === local.active_profile)
      ? local.active_profile
      : profiles[0]?.name ?? "";
  const prof = profiles.find((p) => p.name === activeName);
  const srv = activeServer(prof);
  const mode = local.settings.mode;

  const setMode = async (m: Mode) => {
    if (connected) return;
    await api.setSettings({ ...local.settings, mode: m });
    onChange();
  };
  // VPN membership: "vpn" mode is always on the LAN; any other mode can opt in.
  const join = mode === "vpn" || !!local.settings.join_vpn;
  const setJoin = async (v: boolean) => {
    if (connected) return;
    await api.setSettings({ ...local.settings, join_vpn: v });
    onChange();
  };
  const setActive = async (name: string) => {
    await api.setActiveProfile(name);
    onChange();
  };
  const selectServer = async (name: string) => {
    if (!prof) return;
    await api.saveProfile({ ...prof, selected_server: name });
    onChange();
  };

  // Proxy chain (chosen here on Home, per connection). Hops are the active
  // profile's servers, in order. The display follows the live settings (so the
  // persisted chain shows after load); a ref kept in sync each render lets rapid
  // edits read the latest value without racing the state poll. Every change is
  // persisted immediately, so `connect` uses the chosen chain.
  const chainServers = prof?.servers ?? [];
  const chain = local.settings.chain ?? [];
  const chainRef = useRef<string[]>(chain);
  useEffect(() => { chainRef.current = chain; }, [chain]);
  const mutateChain = (f: (prev: string[]) => string[]) => {
    if (connected) return;
    const c = f(chainRef.current);
    chainRef.current = c;
    api.setSettings({ ...local.settings, chain: c }).then(onChange).catch(() => {});
  };
  const addHop = () => mutateChain((prev) => [...prev, chainServers[0]?.name ?? ""]);
  const setHop = (i: number, name: string) => mutateChain((prev) => prev.map((h, idx) => (idx === i ? name : h)));
  const removeHop = (i: number) => mutateChain((prev) => prev.filter((_, idx) => idx !== i));
  const moveHop = (i: number, d: number) =>
    mutateChain((prev) => {
      const j = i + d;
      if (j < 0 || j >= prev.length) return prev;
      const c = [...prev];
      [c[i], c[j]] = [c[j], c[i]];
      return c;
    });

  const toggle = async () => {
    setErr(null);
    // Global proxy / VPN create a virtual NIC, which needs root/admin. If we're
    // not elevated, ask to relaunch with privileges instead of failing.
    const needsTun = mode === "global_proxy" || mode === "vpn" || join;
    if (!connected && needsTun && elevated === false) {
      setAskElevate(true);
      return;
    }
    setBusy(true);
    try {
      if (connected) await api.disconnect();
      else if (prof) await api.connect(prof.name);
      onChange();
    } catch (e) {
      // connect_profile rejects with the real reason (e.g. TUN needs root).
      setErr(String(e).replace(/^Error:\s*/, ""));
      onChange();
    } finally {
      setBusy(false);
    }
  };

  const testLatency = async () => {
    const servers = prof?.servers ?? [];
    if (!servers.length) return;
    setLatency(Object.fromEntries(servers.map((s) => [s.name, "…"])));
    await Promise.all(
      servers.map(async (s) => {
        try {
          const ms = await api.pingServer(prof!.name, s.name);
          setLatency((p) => ({ ...p, [s.name]: ms }));
        } catch {
          setLatency((p) => ({ ...p, [s.name]: "timeout" }));
        }
      }),
    );
  };

  const numeric = Object.values(latency).filter((v): v is number => typeof v === "number");
  const best = numeric.length ? Math.min(...numeric) : undefined;

  return (
    <>
      <div className="page-head">
        <h2>Home</h2>
        <span className="sub">· local connection</span>
      </div>
      {(err || status.error) && <div className="banner">{err || status.error}</div>}

      {/* Connection hero */}
      <div className="card">
        <div className="hero">
          <button className={"power" + (connected ? " on" : "")} onClick={toggle} disabled={!prof || busy}>
            <span className="glyph">⏻</span>
          </button>
          <div className="state">
            <div className={"big" + (connected ? " on" : "")}>
              {busy ? "Connecting…" : connected ? "Connected" : prof ? "Ready" : "No profile"}
            </div>
            <div className="sub">
              {prof
                ? srv
                  ? <>
                      {MODE_LABEL[mode]} · {prof.name} → <b>{srv.name}</b>{" "}
                      <span className="ip">({srv.host}:{srv.port} · {srv.transport.toUpperCase()})</span>
                    </>
                  : `${prof.name} · no server configured`
                : "Create or import a profile to begin"}
            </div>
            {connected && status.assigned_ip && (
              <div className="sub">virtual IP <span className="ip">{status.assigned_ip}</span></div>
            )}
          </div>
        </div>
      </div>

      {/* Connection: pick profile → server → mode, all by dropdown */}
      <div className="card">
        <div className="section-label">Connection</div>
        <div className="pickers">
          <div className="field">
            <label>Profile</label>
            <select value={activeName} onChange={(e) => setActive(e.target.value)} disabled={connected}>
              {profiles.length === 0 && <option value="">— none —</option>}
              {profiles.map((p) => (
                <option key={p.name} value={p.name}>{p.name}</option>
              ))}
            </select>
          </div>
          <div className="field">
            <label>Server</label>
            <select
              value={srv?.name ?? ""}
              onChange={(e) => selectServer(e.target.value)}
              disabled={connected || !prof || (prof.servers?.length ?? 0) === 0}
            >
              {(!prof || (prof.servers?.length ?? 0) === 0) && <option value="">— none —</option>}
              {prof?.servers?.map((s) => (
                <option key={s.name} value={s.name}>{s.name} · {s.transport.toUpperCase()}</option>
              ))}
            </select>
          </div>
          <div className="field">
            <label>Mode</label>
            <select value={mode} onChange={(e) => setMode(e.target.value as Mode)} disabled={connected}>
              {MODES.map((m) => (
                <option key={m} value={m}>{MODE_LABEL[m]}</option>
              ))}
            </select>
          </div>
        </div>
        <div className="mode-hint">{MODE_HINT[mode]}</div>
        <div className="checks" style={{ marginTop: 12 }}>
          <label>
            <input
              type="checkbox"
              checked={join}
              disabled={connected || mode === "vpn"}
              onChange={(e) => setJoin(e.target.checked)}
            />
            Join this server's VPN LAN
          </label>
        </div>
        <div className="mode-hint" style={{ marginTop: 6 }}>
          {mode === "vpn"
            ? "VPN mode is always on the LAN — you can see and reach the other peers below."
            : join
              ? "Internet still goes through this proxy mode; a LAN-only virtual NIC is added so you can reach other devices by their virtual IP (needs admin/root)."
              : "Optionally reach other devices connected to this same server by their virtual IP."}
        </div>
      </div>

      {/* Proxy chain: choose the hops for this connection */}
      <div className="card">
        <div className="section-label">Proxy chain</div>
        <div className="mode-hint" style={{ marginBottom: chain.length ? 10 : 0 }}>
          {chain.length >= 2
            ? <>Relaying: <b>you → {chain.join(" → ")} → internet</b>. The mode above runs at the last hop.</>
            : "Optional. Relay through 2+ servers in order; leave empty to connect straight to the selected server. Egress mode runs at the last hop; QUIC can only be the first hop."}
        </div>
        {chain.length > 0 && (
          <div className="list">
            {chain.map((h, i) => (
              <div className="list-row" key={i}>
                <span className="tag">{i + 1}</span>
                <div className="field" style={{ flex: 1 }}>
                  <select value={h} onChange={(e) => setHop(i, e.target.value)} disabled={connected}>
                    {chainServers.length === 0 && <option value="">— no servers —</option>}
                    {chainServers.map((sv) => (
                      <option key={sv.name} value={sv.name}>{sv.name} · {sv.transport.toUpperCase()}</option>
                    ))}
                  </select>
                </div>
                <button className="btn ghost sm" onClick={() => moveHop(i, -1)} disabled={connected || i === 0}>↑</button>
                <button className="btn ghost sm" onClick={() => moveHop(i, 1)} disabled={connected || i === chain.length - 1}>↓</button>
                <button className="btn danger sm" onClick={() => removeHop(i)} disabled={connected}>✕</button>
              </div>
            ))}
          </div>
        )}
        <button
          className="btn ghost sm"
          style={{ marginTop: 12 }}
          onClick={addHop}
          disabled={connected || chainServers.length === 0}
        >
          + Add hop
        </button>
      </div>

      {/* Latency */}
      <div className="card">
        <div className="page-head" style={{ margin: 0 }}>
          <div className="section-label" style={{ margin: 0 }}>Server latency</div>
          <div className="grow" />
          <button className="btn ghost sm" onClick={testLatency} disabled={!prof || !(prof.servers?.length)}>
            Test latency
          </button>
        </div>
        <div className="list" style={{ marginTop: 6 }}>
          {(prof?.servers ?? []).map((s) => {
            const v = latency[s.name];
            const isFast = typeof v === "number" && v === best;
            return (
              <div className="list-row" key={s.name}>
                <div className="meta">
                  <div className="n">{s.name}</div>
                  <div className="mono">{s.host}:{s.port}</div>
                </div>
                <span className="tag">{s.transport.toUpperCase()}</span>
                <span className={"latency" + (isFast ? " fast" : "")}>
                  {v === undefined ? "—" : typeof v === "number" ? `${v} ms` : v}
                </span>
              </div>
            );
          })}
          {!(prof?.servers?.length) && <div className="empty">No servers in this profile.</div>}
        </div>
      </div>

      {/* VPN peers — only when connected as a VPN member (vpn mode or joined) */}
      {connected && join && (
        <div className="card">
          <div className="section-label">VPN peers ({status.peers?.length ?? 0})</div>
          <div className="list" style={{ marginTop: 6 }}>
            {(status.peers ?? []).map((p) => (
              <div className="list-row" key={p.ip}>
                <div className="meta">
                  <div className="n">{p.name || "—"}</div>
                </div>
                <span className="ip">{p.ip}</span>
              </div>
            ))}
            {!(status.peers?.length) && (
              <div className="empty">No other peers on this server yet.</div>
            )}
          </div>
        </div>
      )}

      {askElevate && <ElevationModal mode={mode} onClose={() => setAskElevate(false)} />}
    </>
  );
}

function ElevationModal({ mode, onClose }: { mode: Mode; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const relaunch = async () => {
    setBusy(true);
    setErr(null);
    try {
      // Triggers the OS auth dialog (macOS password / Windows UAC). On success
      // the backend relaunches elevated and quits this instance.
      await api.relaunchElevated();
    } catch (e) {
      setErr(String(e).replace(/^Error:\s*/, ""));
      setBusy(false);
    }
  };
  return (
    <div className="modal-bg" onClick={(e) => e.target === e.currentTarget && !busy && onClose()}>
      <div className="modal">
        <h3>Administrator privileges needed</h3>
        <p className="hint">
          <b>{MODE_LABEL[mode]}</b> creates a virtual network device (TUN), which requires
          administrator / root rights. Relaunch EntroTunnel with privileges? You'll be asked
          to authorize it. Your profiles and settings are carried over.
        </p>
        {err && <div className="msg err">{err}</div>}
        <div className="foot">
          <button className="btn ghost" onClick={onClose} disabled={busy}>Cancel</button>
          <button className="btn" onClick={relaunch} disabled={busy}>
            {busy ? "Waiting for authorization…" : "Relaunch as administrator"}
          </button>
        </div>
      </div>
    </div>
  );
}

/* ============================== Profiles ============================== */

const blankServer = (n: number): ServerEntry => ({
  name: n === 1 ? "main" : `server${n}`,
  host: "",
  port: 8443,
  transport: "tcp",
  token: "",
  noise_psk: "",
  tls_skip_verify: false,
});

const blankProfile = (): Profile => ({
  name: "",
  selected_server: "main",
  servers: [blankServer(1)],
});

function Profiles({ profiles, onChange }: { profiles: Profile[]; onChange: () => void }) {
  const [editing, setEditing] = useState<Profile | null>(null);
  // The profile's name when editing began (null = creating a new one). Needed so
  // a rename replaces the original entry instead of adding a second one.
  const [origName, setOrigName] = useState<string | null>(null);
  const [importing, setImporting] = useState(false);
  const [exportLink, setExportLink] = useState<string | null>(null);
  const [tomlExport, setTomlExport] = useState<{ path: string; toml: string; bundle?: boolean } | null>(null);
  const [confirmId, setConfirmId] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const startEdit = (p: Profile) => {
    setEditing({ ...p, servers: p.servers.map((s) => ({ ...s })) });
    setOrigName(p.name);
  };
  const startNew = () => {
    setEditing(blankProfile());
    setOrigName(null);
  };
  const closeEditor = () => {
    setEditing(null);
    setOrigName(null);
  };

  const save = async () => {
    if (!editing) return;
    const name = editing.name.trim();
    if (!name) return; // ProfileEditor surfaces the message
    try {
      await api.saveProfile({ ...editing, name });
      // On rename, drop the entry under the old name (upsert keys on name, so it
      // would otherwise leave the original behind as a duplicate).
      if (origName && origName !== name) {
        await api.removeProfile(origName);
      }
      closeEditor();
      onChange();
    } catch (e) {
      setErr(String(e));
    }
  };

  const doExport = async (name: string) => {
    try {
      setExportLink(await api.exportProfile(name));
    } catch (e) {
      setErr(String(e));
    }
  };

  const doExportToml = async (name: string) => {
    try {
      setTomlExport(await api.exportProfileToml(name));
    } catch (e) {
      setErr(String(e));
    }
  };

  const doExportAll = async () => {
    try {
      setTomlExport({ ...(await api.exportAllProfiles()), bundle: true });
    } catch (e) {
      setErr(String(e));
    }
  };

  const doDelete = async (name: string) => {
    try {
      await api.removeProfile(name);
    } catch (e) {
      setErr(String(e));
    }
    setConfirmId(null);
    onChange();
  };

  if (editing) {
    return (
      <ProfileEditor
        profile={editing}
        setProfile={setEditing}
        onSave={save}
        onCancel={closeEditor}
        originalName={origName}
        existingNames={profiles.map((p) => p.name)}
      />
    );
  }

  return (
    <>
      <div className="page-head">
        <h2>Profiles</h2>
        <span className="sub">· server configurations</span>
        <div className="grow" />
        <div className="btns">
          <button className="btn ghost" onClick={() => setImporting(true)}>Import</button>
          <button className="btn ghost" onClick={doExportAll} disabled={profiles.length === 0}>Export all</button>
          <button className="btn" onClick={startNew}>+ New</button>
        </div>
      </div>

      {err && <div className="banner">{err}</div>}

      <div className="card">
        <div className="list">
          {profiles.map((p) => {
            const a = activeServer(p);
            return (
              <div className="list-row" key={p.name}>
                <div className="meta">
                  <div className="n">{p.name}</div>
                  <div className="s">
                    {p.servers.length} server{p.servers.length === 1 ? "" : "s"}
                    {a ? ` · active: ${a.name}` : ""}
                  </div>
                </div>
                {p.servers.slice(0, 3).map((s) => (
                  <span className="tag" key={s.name}>{s.transport.toUpperCase()}</span>
                ))}
                <button className="btn ghost sm" onClick={() => doExport(p.name)}>Export link</button>
                <button className="btn ghost sm" onClick={() => doExportToml(p.name)}>Export TOML</button>
                <button className="btn ghost sm" onClick={() => startEdit(p)}>Edit</button>
                {confirmId === p.name ? (
                  <>
                    <button className="btn danger sm" onClick={() => doDelete(p.name)}>Confirm</button>
                    <button className="btn ghost sm" onClick={() => setConfirmId(null)}>Cancel</button>
                  </>
                ) : (
                  <button className="btn danger sm" onClick={() => { setErr(null); setConfirmId(p.name); }}>Delete</button>
                )}
              </div>
            );
          })}
          {profiles.length === 0 && (
            <div className="empty">No profiles yet. Click <b>Import</b> to paste a link from the server, or <b>+ New</b>.</div>
          )}
        </div>
      </div>

      {importing && <ImportModal onClose={() => setImporting(false)} onChange={onChange} />}
      {exportLink !== null && <ExportModal link={exportLink} onClose={() => setExportLink(null)} />}
      {tomlExport !== null && <TomlExportModal data={tomlExport} onClose={() => setTomlExport(null)} />}
    </>
  );
}

function ProfileEditor({
  profile,
  setProfile,
  onSave,
  onCancel,
  originalName,
  existingNames,
}: {
  profile: Profile;
  setProfile: (p: Profile) => void;
  onSave: () => void;
  onCancel: () => void;
  originalName: string | null;
  existingNames: string[];
}) {
  const setServer = (i: number, k: keyof ServerEntry, v: unknown) => {
    const servers = profile.servers.map((s, idx) => (idx === i ? { ...s, [k]: v } : s));
    // Renaming a server: if it was the selected one, follow the rename so
    // selected_server doesn't dangle (which would break connect).
    const selected_server =
      k === "name" && profile.selected_server === profile.servers[i].name
        ? (v as string)
        : profile.selected_server;
    setProfile({ ...profile, servers, selected_server });
  };
  const addServer = () =>
    setProfile({ ...profile, servers: [...profile.servers, blankServer(profile.servers.length + 1)] });
  const removeServer = (i: number) => {
    const servers = profile.servers.filter((_, idx) => idx !== i);
    const selected_server =
      profile.selected_server && servers.some((s) => s.name === profile.selected_server)
        ? profile.selected_server
        : servers[0]?.name ?? null;
    setProfile({ ...profile, servers, selected_server });
  };
  const selectedName = profile.selected_server ?? profile.servers[0]?.name;
  const [err, setErr] = useState<string | null>(null);

  const genPsk = async (i: number) => setServer(i, "noise_psk", await api.genPsk());
  const trySave = () => {
    const name = profile.name.trim();
    if (!name) { setErr("Profile name is required"); return; }
    if (name !== originalName && existingNames.includes(name)) {
      setErr(`A profile named "${name}" already exists`);
      return;
    }
    setErr(null);
    onSave();
  };

  return (
    <>
      <div className="page-head">
        <h2>{profile.name ? `Edit ${profile.name}` : "New profile"}</h2>
        <div className="grow" />
        <div className="btns">
          <button className="btn ghost" onClick={onCancel}>Cancel</button>
          <button className="btn" onClick={trySave}>Save</button>
        </div>
      </div>
      {err && <div className="banner">{err}</div>}

      <div className="card">
        <div className="field" style={{ maxWidth: 320 }}>
          <label>Profile name</label>
          <input value={profile.name} onChange={(e) => setProfile({ ...profile, name: e.target.value })} placeholder="home / work / tokyo" />
        </div>
      </div>

      <div className="card">
        <div className="section-label">Servers</div>
        {profile.servers.map((s, i) => (
          <div className="srv" key={i}>
            <div className="srv-head">
              <span className="name">{s.name || `server ${i + 1}`}</span>
              <label className="pill" style={{ textTransform: "none", letterSpacing: 0, cursor: "pointer" }}>
                <input
                  type="radio"
                  name="selected_server"
                  checked={selectedName === s.name}
                  onChange={() => setProfile({ ...profile, selected_server: s.name })}
                  style={{ width: "auto", marginRight: 6 }}
                />
                active
              </label>
              <div className="grow" />
              <button className="btn danger sm" onClick={() => removeServer(i)} disabled={profile.servers.length <= 1}>
                Remove
              </button>
            </div>
            <div className="row">
              <div className="field">
                <label>Name</label>
                <input value={s.name} onChange={(e) => setServer(i, "name", e.target.value)} />
              </div>
              <div className="field">
                <label>Host</label>
                <input value={s.host} onChange={(e) => setServer(i, "host", e.target.value)} placeholder="1.2.3.4 / vpn.example.com" />
              </div>
              <div className="field">
                <label>Port</label>
                <input type="number" value={s.port} onChange={(e) => setServer(i, "port", Number(e.target.value))} />
              </div>
              <div className="field">
                <label>Transport</label>
                <select value={s.transport} onChange={(e) => setServer(i, "transport", e.target.value as Transport)}>
                  <option value="tcp">TCP + Noise</option>
                  <option value="ws">WebSocket (WSS)</option>
                  <option value="quic">QUIC</option>
                </select>
              </div>
            </div>
            <div className="row">
              <div className="field">
                <label>Token</label>
                <input value={s.token} onChange={(e) => setServer(i, "token", e.target.value)} />
              </div>
              <div className="field">
                <label>Noise PSK (base64)</label>
                <div style={{ display: "flex", gap: 8 }}>
                  <input value={s.noise_psk} onChange={(e) => setServer(i, "noise_psk", e.target.value)} />
                  <button className="btn ghost sm" onClick={() => genPsk(i)}>Gen</button>
                </div>
              </div>
            </div>
            <div className="checks" style={{ marginTop: 12 }}>
              <label>
                <input type="checkbox" checked={!!s.tls_skip_verify} onChange={(e) => setServer(i, "tls_skip_verify", e.target.checked)} />
                skip TLS verify (self-signed WSS/QUIC)
              </label>
            </div>
          </div>
        ))}
        <button className="btn ghost" onClick={addServer}>+ Add server</button>
      </div>
    </>
  );
}

function ImportModal({ onClose, onChange }: { onClose: () => void; onChange: () => void }) {
  const [text, setText] = useState("");
  const [msg, setMsg] = useState<{ ok: boolean; text: string } | null>(null);

  const doImport = async () => {
    const t = text.trim();
    try {
      if (t.startsWith("entro://")) {
        // A single shareable link.
        const p = await api.importProfile(t);
        setMsg({ ok: true, text: `Imported “${p.name}” (${p.servers.length} server${p.servers.length === 1 ? "" : "s"})` });
      } else {
        // TOML: a single client.toml (Export TOML) or an all-profiles bundle.
        const n = await api.importProfilesToml(t);
        setMsg({ ok: true, text: `Imported ${n} profile${n === 1 ? "" : "s"}` });
      }
      onChange();
      setTimeout(onClose, 800);
    } catch (e) {
      setMsg({ ok: false, text: String(e).replace(/^Error:\s*/, "") });
    }
  };

  return (
    <div className="modal-bg" onClick={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal">
        <h3>Import profile</h3>
        <p className="hint">
          Paste an <code>entro://…</code> link, or TOML — either a single{" "}
          <code>client.toml</code> (from <b>Export TOML</b>) or an{" "}
          <code>all-profiles.toml</code> bundle (from <b>Export all</b>). Same-name profiles are
          overwritten.
        </p>
        <textarea rows={10} value={text} onChange={(e) => setText(e.target.value)} placeholder={"entro://...\n— or —\n[[profiles]]\nname = ..."} />
        {msg && <div className={"msg " + (msg.ok ? "ok" : "err")}>{msg.text}</div>}
        <div className="foot">
          <button className="btn ghost" onClick={onClose}>Cancel</button>
          <button className="btn" onClick={doImport} disabled={!text.trim()}>Import</button>
        </div>
      </div>
    </div>
  );
}

function ExportModal({ link, onClose }: { link: string; onClose: () => void }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(link);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };
  return (
    <div className="modal-bg" onClick={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal">
        <h3>Share profile</h3>
        <p className="hint">Anyone with this link can connect as this peer. Share it privately.</p>
        <div className="link">{link}</div>
        {copied && <div className="msg ok">Copied to clipboard</div>}
        <div className="foot">
          <button className="btn ghost" onClick={onClose}>Close</button>
          <button className="btn" onClick={copy}>Copy link</button>
        </div>
      </div>
    </div>
  );
}

function TomlExportModal({ data, onClose }: { data: { path: string; toml: string; bundle?: boolean }; onClose: () => void }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(data.toml);
      setCopied(true);
    } catch {
      setCopied(false);
    }
  };
  return (
    <div className="modal-bg" onClick={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal">
        <h3>{data.bundle ? "Export all profiles (TOML)" : "Export config (TOML)"}</h3>
        <p className="hint">
          {data.bundle ? (
            <>All your profiles, saved as one <code>all-profiles.toml</code> bundle (re-import it with
            <b> Import TOML</b>).</>
          ) : (
            <>Full config (servers + current mode/routes), saved as a CLI-ready <code>client.toml</code>.</>
          )}{" "}
          It contains tokens and keys — keep it private.
        </p>
        <div className="link">Saved to: <span className="ip">{data.path}</span></div>
        <textarea rows={12} readOnly value={data.toml} style={{ marginTop: 10 }} />
        {copied && <div className="msg ok">Copied to clipboard</div>}
        <div className="foot">
          <button className="btn ghost" onClick={onClose}>Close</button>
          <button className="btn" onClick={copy}>Copy TOML</button>
        </div>
      </div>
    </div>
  );
}

/* ============================== Settings ============================== */

function SettingsPage({ local, onChange }: { local: LocalState; onChange: () => void }) {
  // Draft initialized once on mount. We deliberately do NOT resync from
  // `local.settings` on every render: the app polls state every 2s, which would
  // otherwise wipe unsaved edits (e.g. a freshly added route) a second after you
  // make them. Navigating away/back remounts this page and re-reads the latest.
  const [s, setS] = useState<ConnectionSettings>(local.settings);
  const [saved, setSaved] = useState(false);

  const set = (k: keyof ConnectionSettings, v: unknown) => setS({ ...s, [k]: v });
  const routes = s.routes ?? [];
  const splitMode = s.split_mode ?? "blacklist";
  const setRoute = (i: number, k: keyof RouteRule, v: unknown) =>
    set("routes", routes.map((r, idx) => (idx === i ? { ...r, [k]: v } : r)));
  // A new rule defaults to the direction that matches the mode: in whitelist the
  // list is what goes THROUGH the tunnel; in blacklist it's what bypasses it.
  const addRoute = () =>
    set("routes", [...routes, { target: "", via: splitMode === "whitelist" ? "tunnel" : "direct" }]);
  const removeRoute = (i: number) => set("routes", routes.filter((_, idx) => idx !== i));

  const save = async () => {
    // Only persist the fields this page manages; keep Home-managed ones (mode,
    // join_vpn, chain) at their latest saved values so a Settings "Save" never
    // clobbers a chain/mode/join chosen on Home.
    await api.setSettings({
      ...local.settings,
      tun_name: s.tun_name,
      http_listen: s.http_listen,
      requested_ip: s.requested_ip,
      client_name: s.client_name,
      routes,
      split_mode: s.split_mode,
      ipv6_killswitch: s.ipv6_killswitch,
    });
    setSaved(true);
    onChange();
    setTimeout(() => setSaved(false), 1200);
  };

  return (
    <>
      <div className="page-head">
        <h2>Settings</h2>
        <span className="sub">· local connection parameters</span>
        <div className="grow" />
        <button className="btn" onClick={save}>Save{saved ? "d ✓" : ""}</button>
      </div>

      <div className="card">
        <div className="section-label">Mode parameters</div>
        <div className="row">
          <div className="field">
            <label>TUN device (global-proxy / VPN)</label>
            <input value={s.tun_name} onChange={(e) => set("tun_name", e.target.value)} />
          </div>
          <div className="field">
            <label>HTTP proxy listen (http-proxy mode)</label>
            <input value={s.http_listen} onChange={(e) => set("http_listen", e.target.value)} />
          </div>
        </div>
        <div className="row" style={{ marginTop: 14 }}>
          <div className="field">
            <label>Requested virtual IP (optional)</label>
            <input value={s.requested_ip ?? ""} onChange={(e) => set("requested_ip", e.target.value || null)} placeholder="server assigns by default" />
          </div>
          <div className="field">
            <label>Device name (shown in admin)</label>
            <input value={s.client_name ?? ""} onChange={(e) => set("client_name", e.target.value || null)} placeholder="my-laptop" />
          </div>
        </div>
      </div>

      <div className="card">
        <div className="section-label">IPv6 leak protection (kill-switch)</div>
        <div className="checks">
          <label>
            <input
              type="checkbox"
              checked={s.ipv6_killswitch ?? true}
              onChange={(e) => set("ipv6_killswitch", e.target.checked)}
            />
            Enable IPv6 kill-switch (recommended)
          </label>
        </div>
        <div className="mode-hint" style={{ marginTop: 6 }}>
          In global-proxy mode, when the server is IPv4-only, block the host's native
          IPv6 so it can't bypass the tunnel and leak your real IP / location
          (IPv6-only sites fall back to the tunneled IPv4). When the server offers IPv6,
          it's tunneled as usual and this has no effect. Turn it off to let your real
          IPv6 connect directly on v4-only servers.
        </div>
      </div>

      <div className="card">
        <div className="section-label">Split-tunnel routes</div>
        <div className="field" style={{ maxWidth: 360, marginBottom: 12 }}>
          <label>Mode (applies to Global proxy)</label>
          <select value={splitMode} onChange={(e) => set("split_mode", e.target.value)}>
            <option value="blacklist">Blacklist — tunnel everything except the rules</option>
            <option value="whitelist">Whitelist — tunnel only the rules</option>
          </select>
        </div>
        <p className="about" style={{ marginBottom: 12 }}>
          {splitMode === "whitelist" ? (
            <>Only listed destinations go through the tunnel; everything else stays direct.
            Add the targets you want tunnelled (<code>via tunnel</code>).</>
          ) : (
            <>Everything goes through the tunnel; listed destinations bypass it.
            Add the targets you want excluded (<code>via direct</code>).</>
          )}{" "}
          A target is a domain (<code>example.com</code>) or IP/CIDR (<code>10.0.0.0/8</code>);
          <code>via</code> can also be a NIC name.
        </p>
        <div className="list">
          {routes.map((r, i) => (
            <div className="list-row" key={i}>
              <div className="field" style={{ flex: 2 }}>
                <input value={r.target} onChange={(e) => setRoute(i, "target", e.target.value)} placeholder="192.168.0.0/16 / example.com" />
              </div>
              <div className="field" style={{ flex: 1 }}>
                <select
                  value={r.via === "direct" || r.via === "tunnel" ? r.via : "__nic__"}
                  onChange={(e) => setRoute(i, "via", e.target.value === "__nic__" ? "" : e.target.value)}
                >
                  <option value="direct">direct (bypass)</option>
                  <option value="tunnel">tunnel (force)</option>
                  <option value="__nic__">interface…</option>
                </select>
              </div>
              {r.via !== "direct" && r.via !== "tunnel" && (
                <div className="field" style={{ flex: 1 }}>
                  <input value={r.via} onChange={(e) => setRoute(i, "via", e.target.value)} placeholder="eth1 / en0" />
                </div>
              )}
              <button className="btn danger sm" onClick={() => removeRoute(i)}>Remove</button>
            </div>
          ))}
          {routes.length === 0 && (
            <div className="empty">
              {splitMode === "whitelist"
                ? "No rules — nothing is tunnelled yet. Add targets to route through the tunnel."
                : "No rules — everything goes through the tunnel."}
            </div>
          )}
        </div>
        <button className="btn ghost" style={{ marginTop: 12 }} onClick={addRoute}>+ Add rule</button>
      </div>
    </>
  );
}

function Logs() {
  const [lines, setLines] = useState<string[]>([]);
  const [follow, setFollow] = useState(true);
  const [filter, setFilter] = useState("");
  const boxRef = useRef<HTMLDivElement>(null);

  // Poll the backend's captured tracing buffer (client engine + app both log here).
  useEffect(() => {
    let alive = true;
    const pull = async () => {
      try {
        const l = await api.getLogs();
        if (alive) setLines(l);
      } catch {
        /* outside Tauri */
      }
    };
    pull();
    const t = setInterval(pull, 1000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, []);

  const shown = filter
    ? lines.filter((l) => l.toLowerCase().includes(filter.toLowerCase()))
    : lines;

  // Auto-scroll to the newest line while "follow" is on.
  useEffect(() => {
    if (follow && boxRef.current) boxRef.current.scrollTop = boxRef.current.scrollHeight;
  }, [shown, follow]);

  const copy = () => navigator.clipboard?.writeText(shown.join("\n")).catch(() => {});

  return (
    <>
      <div className="page-head">
        <h2>Logs</h2>
        <span className="sub">· live ({shown.length})</span>
        <div className="grow" />
        <input
          className="log-filter"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="filter…"
        />
        <label className="check-inline">
          <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} /> Follow
        </label>
        <button className="btn ghost sm" onClick={copy} disabled={!shown.length}>Copy</button>
      </div>
      <div className="card">
        <div className="logs" ref={boxRef}>
          {shown.length === 0 ? (
            <div className="empty">No logs yet. Connect a profile to see engine activity.</div>
          ) : (
            shown.map((l, i) => (
              <div className="log-line" key={i}>{l}</div>
            ))
          )}
        </div>
      </div>
    </>
  );
}
