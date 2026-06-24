import { useCallback, useEffect, useMemo, useRef, useState, type ComponentType, type ReactNode } from "react";
import logoUrl from "./logo.svg";
import {
  IconArrowDown,
  IconArrowUp,
  IconClose,
  IconConnections,
  IconDashboard,
  IconDownload,
  IconEdit,
  IconExport,
  IconImport,
  IconLogs,
  IconMoon,
  IconPlus,
  IconPower,
  IconProfiles,
  IconProxy,
  IconRules,
  IconServer,
  IconSettings,
  IconShield,
  IconSun,
  IconTrash,
  IconUpload,
  IconWifi,
} from "./icons";
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

type Page = "dashboard" | "proxies" | "profiles" | "connections" | "rules" | "logs" | "settings";
type NavIcon = ComponentType<{ size?: number; className?: string }>;
type Theme = "light" | "dark";

function getInitialTheme(): Theme {
  try {
    const saved = localStorage.getItem("et-theme");
    if (saved === "light" || saved === "dark") return saved;
    if (window.matchMedia?.("(prefers-color-scheme: light)").matches) return "light";
  } catch {
    /* localStorage / matchMedia unavailable */
  }
  return "dark";
}

type TrafficSample = {
  at: number;
  upRate: number;
  downRate: number;
  upTotal: number;
  downTotal: number;
};

const NAV: { id: Page; Icon: NavIcon; label: string }[] = [
  { id: "dashboard", Icon: IconDashboard, label: "Dashboard" },
  { id: "proxies", Icon: IconProxy, label: "Proxies" },
  { id: "profiles", Icon: IconProfiles, label: "Profiles" },
  { id: "connections", Icon: IconConnections, label: "Connections" },
  { id: "rules", Icon: IconRules, label: "Rules" },
  { id: "logs", Icon: IconLogs, label: "Logs" },
  { id: "settings", Icon: IconSettings, label: "Settings" },
];

const MODES: Mode[] = ["global_proxy", "system_proxy", "http_proxy", "vpn"];

const DEFAULT_STATE: LocalState = {
  settings: {
    mode: "global_proxy",
    tun_name: "et0",
    http_listen: "127.0.0.1:7890",
    routes: [],
    split_mode: "blacklist",
    ipv6_killswitch: true,
  },
  active_profile: null,
};

const MODE_SHORT: Record<Mode, string> = {
  global_proxy: "Global",
  system_proxy: "System",
  http_proxy: "HTTP",
  vpn: "VPN",
};

function activeProfileName(local: LocalState, profiles: Profile[]) {
  return local.active_profile && profiles.some((p) => p.name === local.active_profile)
    ? local.active_profile
    : profiles[0]?.name ?? "";
}

function formatBytes(n: number | undefined) {
  let v = Math.max(0, Number(n ?? 0));
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  const digits = i === 0 ? 0 : v < 10 ? 2 : 1;
  return `${v.toFixed(digits)} ${units[i]}`;
}

function formatRate(n: number | undefined) {
  return `${formatBytes(n)}/s`;
}

// Latency colour by absolute round-trip: green < 250ms, yellow 250-500ms,
// red > 500ms (and for "timeout"). "testing"/"not tested" stay neutral.
function latencyTone(v: number | string | undefined): "good" | "warn" | "bad" | "" {
  if (v === "timeout") return "bad";
  if (typeof v !== "number") return "";
  if (v < 250) return "good";
  if (v <= 500) return "warn";
  return "bad";
}

function currentSample(samples: TrafficSample[]) {
  return samples[samples.length - 1] ?? { at: Date.now(), upRate: 0, downRate: 0, upTotal: 0, downTotal: 0 };
}

export default function App() {
  const [page, setPage] = useState<Page>("dashboard");
  const [profiles, setProfiles] = useState<Profile[]>([]);
  const [status, setStatus] = useState<Status>({ connected: false, up_bytes: 0, down_bytes: 0 });
  const [local, setLocal] = useState<LocalState>(DEFAULT_STATE);
  const [elevated, setElevated] = useState<boolean | null>(null);
  const [traffic, setTraffic] = useState<TrafficSample[]>([]);
  const [theme, setTheme] = useState<Theme>(getInitialTheme);
  const lastTraffic = useRef<{ at: number; up: number; down: number; connected: boolean } | null>(null);

  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    try { localStorage.setItem("et-theme", theme); } catch { /* ignore */ }
  }, [theme]);
  const toggleTheme = () => setTheme((t) => (t === "dark" ? "light" : "dark"));

  const refresh = useCallback(async () => {
    try {
      const [p, s, st, el] = await Promise.all([
        api.listProfiles(),
        api.status(),
        api.getState(),
        api.isElevated().catch(() => null),
      ]);
      setProfiles(p);
      setStatus({ up_bytes: 0, down_bytes: 0, ...s });
      setLocal(st ?? DEFAULT_STATE);
      setElevated(el);
    } catch {
      /* running outside Tauri (plain vite) -- keep the static shell usable */
    }
  }, []);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 1000);
    return () => clearInterval(t);
  }, [refresh]);

  useEffect(() => {
    const now = Date.now();
    const up = status.up_bytes ?? 0;
    const down = status.down_bytes ?? 0;
    const prev = lastTraffic.current;
    let upRate = 0;
    let downRate = 0;
    if (status.connected && prev?.connected && up >= prev.up && down >= prev.down) {
      const dt = Math.max(0.25, (now - prev.at) / 1000);
      upRate = (up - prev.up) / dt;
      downRate = (down - prev.down) / dt;
    }
    lastTraffic.current = { at: now, up, down, connected: status.connected };
    setTraffic((old) => {
      if (!status.connected) return [];
      return [...old, { at: now, upRate, downRate, upTotal: up, downTotal: down }].slice(-72);
    });
  }, [status.connected, status.up_bytes, status.down_bytes]);

  return (
    <div className="app-shell">
      <Sidebar page={page} setPage={setPage} status={status} traffic={traffic} theme={theme} toggleTheme={toggleTheme} />
      <main className="main-pane">
        {page === "dashboard" && (
          <Dashboard
            status={status}
            profiles={profiles}
            local={local}
            elevated={elevated}
            traffic={traffic}
            onChange={refresh}
            goProfiles={() => setPage("profiles")}
          />
        )}
        {page === "proxies" && <Proxies profiles={profiles} local={local} status={status} onChange={refresh} />}
        {page === "profiles" && <Profiles profiles={profiles} onChange={refresh} />}
        {page === "connections" && <Connections profiles={profiles} local={local} status={status} onChange={refresh} />}
        {page === "rules" && <RulesPage local={local} onChange={refresh} />}
        {page === "logs" && <Logs />}
        {page === "settings" && <SettingsPage local={local} onChange={refresh} />}
      </main>
    </div>
  );
}

function Sidebar({
  page,
  setPage,
  status,
  traffic,
  theme,
  toggleTheme,
}: {
  page: Page;
  setPage: (p: Page) => void;
  status: Status;
  traffic: TrafficSample[];
  theme: Theme;
  toggleTheme: () => void;
}) {
  const sample = currentSample(traffic);
  return (
    <aside className="sidebar">
      <div className="brand">
        <img className="logo" src={logoUrl} alt="EntroTunnel" />
        <div>
          <h1>EntroTunnel</h1>
          <span>Desktop Client</span>
        </div>
      </div>
      <nav className="nav">
        {NAV.map(({ id, Icon, label }) => (
          <button key={id} className={page === id ? "active" : ""} onClick={() => setPage(id)}>
            <Icon size={18} />
            <span>{label}</span>
          </button>
        ))}
      </nav>
      <div className="sidebar-fill" />
      <button className="theme-toggle" onClick={toggleTheme} title="Toggle light / dark mode" aria-label="Toggle light / dark mode">
        {theme === "dark" ? <IconSun size={16} /> : <IconMoon size={16} />}
        <span>{theme === "dark" ? "Light mode" : "Dark mode"}</span>
      </button>
      <div className="side-traffic">
        <div className="side-row">
          <span>Traffic</span>
          <span className={status.connected ? "dot on" : "dot"} />
        </div>
        <MiniTrafficChart samples={traffic} />
        <div className="speed-pair">
          <span><IconUpload size={14} />{formatRate(sample.upRate)}</span>
          <span><IconDownload size={14} />{formatRate(sample.downRate)}</span>
        </div>
      </div>
      <div className={"status-chip" + (status.connected ? " on" : "")}> 
        <span className="dot" />
        <div>
          <b>{status.connected ? "Connected" : "Disconnected"}</b>
          <small>{status.assigned_ip ?? status.profile ?? "No active tunnel"}</small>
        </div>
      </div>
    </aside>
  );
}

function Dashboard({
  status,
  profiles,
  local,
  elevated,
  traffic,
  onChange,
  goProfiles,
}: {
  status: Status;
  profiles: Profile[];
  local: LocalState;
  elevated: boolean | null;
  traffic: TrafficSample[];
  onChange: () => void;
  goProfiles: () => void;
}) {
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [askElevate, setAskElevate] = useState(false);
  const activeName = activeProfileName(local, profiles);
  const prof = profiles.find((p) => p.name === activeName);
  const srv = activeServer(prof);
  const mode = local.settings.mode;
  const connected = status.connected;
  const join = mode === "vpn" || !!local.settings.join_vpn;
  const sample = currentSample(traffic);

  const setMode = async (m: Mode) => {
    if (connected) return;
    await api.setSettings({ ...local.settings, mode: m });
    onChange();
  };
  const setJoin = async (v: boolean) => {
    if (connected) return;
    await api.setSettings({ ...local.settings, join_vpn: v });
    onChange();
  };
  const setActive = async (name: string) => {
    await api.setActiveProfile(name || null);
    onChange();
  };
  const selectServer = async (name: string) => {
    if (!prof) return;
    await api.saveProfile({ ...prof, selected_server: name });
    onChange();
  };

  const toggle = async () => {
    setErr(null);
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
      setErr(String(e).replace(/^Error:\s*/, ""));
      onChange();
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <PageTitle title="Dashboard" subtitle="Clash-style local tunnel control" />
      {(err || status.error) && <div className="banner">{err || status.error}</div>}

      <section className="dashboard-grid">
        <div className="panel connect-panel span-2">
          <div className="connect-left">
            <button className={"power" + (connected ? " on" : "")} onClick={toggle} disabled={!prof || busy}>
              <IconPower size={42} />
            </button>
            <div>
              <div className="eyebrow">Current Session</div>
              <h2>{busy ? "Connecting" : connected ? "Connected" : prof ? "Ready" : "No profile"}</h2>
              <p>
                {prof && srv
                  ? `${prof.name} / ${srv.name} / ${srv.host}:${srv.port}`
                  : "Create or import a profile to begin."}
              </p>
              {connected && status.assigned_ip && <p className="mono-line">Virtual IP {status.assigned_ip}</p>}
            </div>
          </div>
          <div className="quick-pickers">
            <label>
              Profile
              <select value={activeName} onChange={(e) => setActive(e.target.value)} disabled={connected}>
                {profiles.length === 0 && <option value="">none</option>}
                {profiles.map((p) => <option key={p.name} value={p.name}>{p.name}</option>)}
              </select>
            </label>
            <label>
              Server
              <select value={srv?.name ?? ""} onChange={(e) => selectServer(e.target.value)} disabled={connected || !prof}>
                {(!prof || prof.servers.length === 0) && <option value="">none</option>}
                {prof?.servers.map((s) => <option key={s.name} value={s.name}>{s.name} / {s.transport.toUpperCase()}</option>)}
              </select>
            </label>
          </div>
        </div>

        <div className="panel traffic-panel span-2">
          <PanelHead icon={<IconWifi size={19} />} title="Traffic Statistics" hint="Current session" />
          <TrafficChart samples={traffic} id="dashboard-traffic" />
          <div className="metric-grid four">
            <MetricCard label="Upload" value={formatBytes(status.up_bytes)} hint={formatRate(sample.upRate)} icon={<IconUpload />} tone="up" />
            <MetricCard label="Download" value={formatBytes(status.down_bytes)} hint={formatRate(sample.downRate)} icon={<IconDownload />} tone="down" />
            <MetricCard label="Mode" value={MODE_SHORT[mode]} hint={connected ? "active" : "standby"} icon={<IconShield />} />
            <MetricCard label="Peers" value={String(status.peers?.length ?? 0)} hint={join ? "VPN LAN" : "not joined"} icon={<IconConnections />} />
          </div>
        </div>

        <div className="panel">
          <PanelHead icon={<IconProfiles size={19} />} title="Profiles" hint={`${profiles.length} configured`} />
          <div className="large-value">{prof?.name ?? "No profile"}</div>
          <p className="panel-copy">Subscriptions and server credentials stay separate from local connection mode.</p>
          <button className="btn ghost wide" onClick={goProfiles}>Manage profiles</button>
        </div>

        <div className="panel">
          <PanelHead icon={<IconProxy size={19} />} title="Proxy Mode" hint={connected ? "locked" : "switch anytime"} />
          <div className="mode-cards compact">
            {MODES.map((m) => (
              <button key={m} className={mode === m ? "active" : ""} onClick={() => setMode(m)} disabled={connected}>
                <span>{MODE_SHORT[m]}</span>
                <small>{m === "global_proxy" ? "TUN" : m === "system_proxy" ? "OS" : m === "http_proxy" ? "7890" : "LAN"}</small>
              </button>
            ))}
          </div>
          <p className="mode-hint">{MODE_HINT[mode]}</p>
        </div>

        <div className="panel">
          <PanelHead icon={<IconSettings size={19} />} title="Network" hint={local.settings.http_listen} />
          <div className="switch-row">
            <div>
              <b>Join VPN LAN</b>
              <span>Reach peers by virtual IP.</span>
            </div>
            <label className="switch">
              <input type="checkbox" checked={join} disabled={connected || mode === "vpn"} onChange={(e) => setJoin(e.target.checked)} />
              <span />
            </label>
          </div>
          <div className="mini-list">
            <span>TUN device</span><b>{local.settings.tun_name}</b>
            <span>HTTP proxy</span><b>{local.settings.http_listen}</b>
          </div>
        </div>

        <div className="panel">
          <PanelHead icon={<IconServer size={19} />} title="Active Server" hint={srv?.transport.toUpperCase() ?? "none"} />
          <div className="large-value">{srv?.name ?? "No server"}</div>
          <p className="panel-copy mono-line">{srv ? `${srv.host}:${srv.port}` : "Add a server inside Profiles."}</p>
          <div className="tag-row">
            {srv && <span className="tag accent">{srv.transport.toUpperCase()}</span>}
            {srv?.tls_skip_verify && <span className="tag warn">TLS skip verify</span>}
          </div>
        </div>
      </section>

      {askElevate && <ElevationModal mode={mode} onClose={() => setAskElevate(false)} />}
    </>
  );
}

function Proxies({ profiles, local, status, onChange }: { profiles: Profile[]; local: LocalState; status: Status; onChange: () => void }) {
  const [latency, setLatency] = useState<Record<string, number | string>>({});
  const [busy, setBusy] = useState(false);
  const activeName = activeProfileName(local, profiles);
  const prof = profiles.find((p) => p.name === activeName);
  const srv = activeServer(prof);
  const servers = prof?.servers ?? [];

  const setActive = async (name: string) => {
    await api.setActiveProfile(name || null);
    onChange();
  };
  const selectServer = async (name: string) => {
    if (!prof || status.connected) return;
    await api.saveProfile({ ...prof, selected_server: name });
    onChange();
  };
  const testLatency = async () => {
    if (!prof || !servers.length) return;
    setBusy(true);
    setLatency(Object.fromEntries(servers.map((s) => [s.name, "testing"])));
    await Promise.all(servers.map(async (s) => {
      try {
        const ms = await api.pingServer(prof.name, s.name);
        setLatency((p) => ({ ...p, [s.name]: ms }));
      } catch {
        setLatency((p) => ({ ...p, [s.name]: "timeout" }));
      }
    }));
    setBusy(false);
  };

  return (
    <>
      <PageTitle title="Proxies" subtitle="Select nodes and test latency">
        <button className="btn ghost with-icon" onClick={testLatency} disabled={!prof || !servers.length || busy}><IconWifi size={16} />{busy ? "Testing" : "Test latency"}</button>
      </PageTitle>
      <div className="panel toolbar-panel">
        <label>
          Active profile
          <select value={activeName} onChange={(e) => setActive(e.target.value)}>
            {profiles.length === 0 && <option value="">none</option>}
            {profiles.map((p) => <option key={p.name} value={p.name}>{p.name}</option>)}
          </select>
        </label>
        <div className="toolbar-status">
          <span className={status.connected ? "dot on" : "dot"} />
          {status.connected ? `Locked on ${srv?.name ?? "selected server"}` : "Choose the egress server before connecting"}
        </div>
      </div>
      <div className="server-grid">
        {servers.map((s) => {
          const v = latency[s.name];
          const isActive = srv?.name === s.name;
          return (
            <button key={s.name} className={"server-card" + (isActive ? " active" : "")} onClick={() => selectServer(s.name)} disabled={status.connected}>
              <div className="server-main">
                <IconServer size={22} />
                <div>
                  <b>{s.name}</b>
                  <span>{s.host}:{s.port}</span>
                </div>
              </div>
              <div className="server-foot">
                <span className="tag">{s.transport.toUpperCase()}</span>
                <span className={"latency " + latencyTone(v)}>{v === undefined ? "not tested" : typeof v === "number" ? `${v} ms` : v}</span>
              </div>
            </button>
          );
        })}
        {servers.length === 0 && <EmptyState text="No servers in the active profile." />}
      </div>
    </>
  );
}

function Connections({ profiles, local, status, onChange }: { profiles: Profile[]; local: LocalState; status: Status; onChange: () => void }) {
  const activeName = activeProfileName(local, profiles);
  const prof = profiles.find((p) => p.name === activeName);
  const chainServers = prof?.servers ?? [];
  const chain = local.settings.chain ?? [];
  const chainRef = useRef<string[]>(chain);
  useEffect(() => { chainRef.current = chain; }, [chain]);

  const mutateChain = (f: (prev: string[]) => string[]) => {
    if (status.connected) return;
    const c = f(chainRef.current);
    chainRef.current = c;
    api.setSettings({ ...local.settings, chain: c }).then(onChange).catch(() => {});
  };
  const addHop = () => mutateChain((prev) => [...prev, chainServers[0]?.name ?? ""]);
  const setHop = (i: number, name: string) => mutateChain((prev) => prev.map((h, idx) => (idx === i ? name : h)));
  const removeHop = (i: number) => mutateChain((prev) => prev.filter((_, idx) => idx !== i));
  const moveHop = (i: number, d: number) => mutateChain((prev) => {
    const j = i + d;
    if (j < 0 || j >= prev.length) return prev;
    const c = [...prev];
    [c[i], c[j]] = [c[j], c[i]];
    return c;
  });

  return (
    <>
      <PageTitle title="Connections" subtitle="Relay chain and VPN peers" />
      <div className="connection-layout">
        <div className="panel span-2">
          <PanelHead icon={<IconConnections size={19} />} title="Proxy Chain" hint={chain.length >= 2 ? "multi-hop" : "direct"} />
          <p className="panel-copy">
            {chain.length >= 2
              ? `Relaying through ${chain.join(" to ")}. The selected mode runs at the last hop.`
              : "Optional. Add two or more hops to relay through multiple servers."}
          </p>
          <div className="chain-list">
            {chain.map((h, i) => (
              <div className="chain-row" key={i}>
                <span className="hop-index">{i + 1}</span>
                <select value={h} onChange={(e) => setHop(i, e.target.value)} disabled={status.connected}>
                  {chainServers.length === 0 && <option value="">no servers</option>}
                  {chainServers.map((sv) => <option key={sv.name} value={sv.name}>{sv.name} / {sv.transport.toUpperCase()}</option>)}
                </select>
                <button className="btn icon-only ghost" onClick={() => moveHop(i, -1)} disabled={status.connected || i === 0} aria-label="Move up"><IconArrowUp size={16} /></button>
                <button className="btn icon-only ghost" onClick={() => moveHop(i, 1)} disabled={status.connected || i === chain.length - 1} aria-label="Move down"><IconArrowDown size={16} /></button>
                <button className="btn icon-only danger" onClick={() => removeHop(i)} disabled={status.connected} aria-label="Remove hop"><IconClose size={16} /></button>
              </div>
            ))}
            {chain.length === 0 && <EmptyState text="No relay hops configured." />}
          </div>
          <button className="btn ghost with-icon" onClick={addHop} disabled={status.connected || chainServers.length === 0}><IconPlus size={16} />Add hop</button>
        </div>

        <div className="panel">
          <PanelHead icon={<IconWifi size={19} />} title="Session" hint={status.connected ? "online" : "offline"} />
          <div className="session-stack">
            <MetricLine label="Profile" value={status.profile ?? (activeName || "none")} />
            <MetricLine label="Mode" value={local.settings.mode} />
            <MetricLine label="Virtual IP" value={status.assigned_ip ?? "not assigned"} />
            <MetricLine label="Uploaded" value={formatBytes(status.up_bytes)} />
            <MetricLine label="Downloaded" value={formatBytes(status.down_bytes)} />
          </div>
        </div>

        <div className="panel span-3">
          <PanelHead icon={<IconShield size={19} />} title={`VPN Peers (${status.peers?.length ?? 0})`} hint="same server LAN" />
          <div className="peer-grid">
            {(status.peers ?? []).map((p) => (
              <div className="peer-card" key={p.ip}>
                <IconConnections size={18} />
                <b>{p.name || "Unnamed peer"}</b>
                <span>{p.ip}</span>
              </div>
            ))}
            {!(status.peers?.length) && <EmptyState text="No other VPN peers are visible right now." />}
          </div>
        </div>
      </div>
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
        <p className="hint"><b>{MODE_LABEL[mode]}</b> creates a virtual network device and needs administrator or root rights.</p>
        {err && <div className="msg err">{err}</div>}
        <div className="foot">
          <button className="btn ghost" onClick={onClose} disabled={busy}>Cancel</button>
          <button className="btn" onClick={relaunch} disabled={busy}>{busy ? "Waiting for authorization" : "Relaunch as administrator"}</button>
        </div>
      </div>
    </div>
  );
}

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
    if (!name) return;
    try {
      await api.saveProfile({ ...editing, name });
      if (origName && origName !== name) await api.removeProfile(origName);
      closeEditor();
      onChange();
    } catch (e) {
      setErr(String(e));
    }
  };
  const doExport = async (name: string) => {
    try { setExportLink(await api.exportProfile(name)); } catch (e) { setErr(String(e)); }
  };
  const doExportToml = async (name: string) => {
    try { setTomlExport(await api.exportProfileToml(name)); } catch (e) { setErr(String(e)); }
  };
  const doExportAll = async () => {
    try { setTomlExport({ ...(await api.exportAllProfiles()), bundle: true }); } catch (e) { setErr(String(e)); }
  };
  const doDelete = async (name: string) => {
    try { await api.removeProfile(name); } catch (e) { setErr(String(e)); }
    setConfirmId(null);
    onChange();
  };

  if (editing) {
    return (
      <ProfileEditor
        profile={editing}
        setProfile={(p) => setEditing(p)}
        onSave={save}
        onCancel={closeEditor}
        originalName={origName}
        existingNames={profiles.map((p) => p.name)}
      />
    );
  }

  return (
    <>
      <PageTitle title="Profiles" subtitle="Subscriptions and server configurations">
        <button className="btn ghost with-icon" onClick={() => setImporting(true)}><IconImport size={16} />Import</button>
        <button className="btn ghost with-icon" onClick={doExportAll} disabled={profiles.length === 0}><IconExport size={16} />Export all</button>
        <button className="btn with-icon" onClick={startNew}><IconPlus size={16} />New profile</button>
      </PageTitle>
      {err && <div className="banner">{err}</div>}
      <div className="profile-grid">
        {profiles.map((p) => {
          const a = activeServer(p);
          return (
            <div className="profile-card" key={p.name}>
              <div className="profile-top">
                <IconProfiles size={22} />
                <div>
                  <h3>{p.name}</h3>
                  <span>{p.servers.length} server{p.servers.length === 1 ? "" : "s"}{a ? ` / active ${a.name}` : ""}</span>
                </div>
              </div>
              <div className="tag-row">
                {p.servers.slice(0, 4).map((s) => <span className="tag" key={s.name}>{s.transport.toUpperCase()}</span>)}
              </div>
              <div className="card-actions">
                <button className="btn ghost sm" onClick={() => doExport(p.name)}>Link</button>
                <button className="btn ghost sm" onClick={() => doExportToml(p.name)}>TOML</button>
                <button className="btn ghost sm with-icon" onClick={() => startEdit(p)}><IconEdit size={14} />Edit</button>
                {confirmId === p.name ? (
                  <>
                    <button className="btn danger sm" onClick={() => doDelete(p.name)}>Confirm</button>
                    <button className="btn ghost sm" onClick={() => setConfirmId(null)}>Cancel</button>
                  </>
                ) : (
                  <button className="btn danger sm with-icon" onClick={() => { setErr(null); setConfirmId(p.name); }}><IconTrash size={14} />Delete</button>
                )}
              </div>
            </div>
          );
        })}
        {profiles.length === 0 && <EmptyState text="No profiles yet. Import a link or create a new profile." />}
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
  const [err, setErr] = useState<string | null>(null);
  const selectedName = profile.selected_server ?? profile.servers[0]?.name;
  const setServer = (i: number, k: keyof ServerEntry, v: unknown) => {
    const servers = profile.servers.map((s, idx) => (idx === i ? { ...s, [k]: v } : s));
    const selected_server = k === "name" && profile.selected_server === profile.servers[i].name ? (v as string) : profile.selected_server;
    setProfile({ ...profile, servers, selected_server });
  };
  const addServer = () => setProfile({ ...profile, servers: [...profile.servers, blankServer(profile.servers.length + 1)] });
  const removeServer = (i: number) => {
    const servers = profile.servers.filter((_, idx) => idx !== i);
    const selected_server = profile.selected_server && servers.some((s) => s.name === profile.selected_server) ? profile.selected_server : servers[0]?.name ?? null;
    setProfile({ ...profile, servers, selected_server });
  };
  const genPsk = async (i: number) => setServer(i, "noise_psk", await api.genPsk());
  const trySave = () => {
    const name = profile.name.trim();
    if (!name) { setErr("Profile name is required"); return; }
    if (name !== originalName && existingNames.includes(name)) { setErr(`A profile named "${name}" already exists`); return; }
    setErr(null);
    onSave();
  };

  return (
    <>
      <PageTitle title={profile.name ? `Edit ${profile.name}` : "New profile"} subtitle="Server endpoints and credentials">
        <button className="btn ghost" onClick={onCancel}>Cancel</button>
        <button className="btn" onClick={trySave}>Save</button>
      </PageTitle>
      {err && <div className="banner">{err}</div>}
      <div className="panel form-panel">
        <label className="field wide-field">Profile name<input value={profile.name} onChange={(e) => setProfile({ ...profile, name: e.target.value })} placeholder="home / work / tokyo" /></label>
      </div>
      <div className="server-editor-list">
        {profile.servers.map((s, i) => (
          <div className="panel server-editor" key={i}>
            <div className="editor-head">
              <div className="server-main"><IconServer size={21} /><b>{s.name || `server ${i + 1}`}</b></div>
              <label className="radio-pill"><input type="radio" name="selected_server" checked={selectedName === s.name} onChange={() => setProfile({ ...profile, selected_server: s.name })} />Active</label>
              <button className="btn danger sm with-icon" onClick={() => removeServer(i)} disabled={profile.servers.length <= 1}><IconTrash size={14} />Remove</button>
            </div>
            <div className="row">
              <label className="field">Name<input value={s.name} onChange={(e) => setServer(i, "name", e.target.value)} /></label>
              <label className="field">Host<input value={s.host} onChange={(e) => setServer(i, "host", e.target.value)} placeholder="1.2.3.4 / vpn.example.com" /></label>
              <label className="field">Port<input type="number" value={s.port} onChange={(e) => setServer(i, "port", Number(e.target.value))} /></label>
              <label className="field">Transport<select value={s.transport} onChange={(e) => setServer(i, "transport", e.target.value as Transport)}><option value="tcp">TCP + Noise</option><option value="ws">WebSocket (WSS)</option><option value="quic">QUIC</option></select></label>
            </div>
            <div className="row two">
              <label className="field">Token<input value={s.token} onChange={(e) => setServer(i, "token", e.target.value)} /></label>
              <label className="field">Noise PSK (base64)<span className="inline-input"><input value={s.noise_psk} onChange={(e) => setServer(i, "noise_psk", e.target.value)} /><button className="btn ghost sm" onClick={() => genPsk(i)}>Generate</button></span></label>
            </div>
            <div className="checks"><label><input type="checkbox" checked={!!s.tls_skip_verify} onChange={(e) => setServer(i, "tls_skip_verify", e.target.checked)} />Skip TLS verify for self-signed WSS/QUIC</label></div>
          </div>
        ))}
      </div>
      <button className="btn ghost with-icon" onClick={addServer}><IconPlus size={16} />Add server</button>
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
        const p = await api.importProfile(t);
        setMsg({ ok: true, text: `Imported ${p.name} (${p.servers.length} server${p.servers.length === 1 ? "" : "s"})` });
      } else {
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
      <div className="modal wide-modal">
        <h3>Import profile</h3>
        <p className="hint">Paste an entro:// link or TOML bundle. Same-name profiles are overwritten.</p>
        <textarea rows={10} value={text} onChange={(e) => setText(e.target.value)} placeholder={"entro://...\n\n[[profiles]]\nname = ..."} />
        {msg && <div className={"msg " + (msg.ok ? "ok" : "err")}>{msg.text}</div>}
        <div className="foot"><button className="btn ghost" onClick={onClose}>Cancel</button><button className="btn" onClick={doImport} disabled={!text.trim()}>Import</button></div>
      </div>
    </div>
  );
}

function ExportModal({ link, onClose }: { link: string; onClose: () => void }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try { await navigator.clipboard.writeText(link); setCopied(true); } catch { setCopied(false); }
  };
  return (
    <div className="modal-bg" onClick={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal wide-modal">
        <h3>Share profile</h3>
        <p className="hint">Anyone with this link can connect as this peer. Share it privately.</p>
        <div className="link-box">{link}</div>
        {copied && <div className="msg ok">Copied to clipboard</div>}
        <div className="foot"><button className="btn ghost" onClick={onClose}>Close</button><button className="btn" onClick={copy}>Copy link</button></div>
      </div>
    </div>
  );
}

function TomlExportModal({ data, onClose }: { data: { path: string; toml: string; bundle?: boolean }; onClose: () => void }) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    try { await navigator.clipboard.writeText(data.toml); setCopied(true); } catch { setCopied(false); }
  };
  return (
    <div className="modal-bg" onClick={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal wide-modal">
        <h3>{data.bundle ? "Export all profiles (TOML)" : "Export config (TOML)"}</h3>
        <p className="hint">Saved to <span className="mono-line">{data.path}</span>. It contains tokens and keys.</p>
        <textarea rows={12} readOnly value={data.toml} />
        {copied && <div className="msg ok">Copied to clipboard</div>}
        <div className="foot"><button className="btn ghost" onClick={onClose}>Close</button><button className="btn" onClick={copy}>Copy TOML</button></div>
      </div>
    </div>
  );
}

function RulesPage({ local, onChange }: { local: LocalState; onChange: () => void }) {
  const [s, setS] = useState<ConnectionSettings>(local.settings);
  const [saved, setSaved] = useState(false);
  const routes = s.routes ?? [];
  const splitMode = s.split_mode ?? "blacklist";
  const set = (k: keyof ConnectionSettings, v: unknown) => setS({ ...s, [k]: v });
  const setRoute = (i: number, k: keyof RouteRule, v: unknown) => set("routes", routes.map((r, idx) => (idx === i ? { ...r, [k]: v } : r)));
  const addRoute = () => set("routes", [...routes, { target: "", via: splitMode === "whitelist" ? "tunnel" : "direct" }]);
  const removeRoute = (i: number) => set("routes", routes.filter((_, idx) => idx !== i));
  const save = async () => {
    await api.setSettings({ ...local.settings, routes, split_mode: s.split_mode });
    setSaved(true);
    onChange();
    setTimeout(() => setSaved(false), 1200);
  };

  return (
    <>
      <PageTitle title="Rules" subtitle="Split tunnel routing">
        <button className="btn" onClick={save}>{saved ? "Saved" : "Save rules"}</button>
      </PageTitle>
      <div className="panel form-panel">
        <label className="field wide-field">Global proxy policy<select value={splitMode} onChange={(e) => set("split_mode", e.target.value)}><option value="blacklist">Blacklist - tunnel everything except rules</option><option value="whitelist">Whitelist - tunnel only listed rules</option></select></label>
        <p className="panel-copy">Targets can be domains, IPv4 addresses, or CIDR ranges. Route via direct, tunnel, or a specific interface name.</p>
      </div>
      <div className="panel">
        <PanelHead icon={<IconRules size={19} />} title="Routing Rules" hint={`${routes.length} rules`} />
        <div className="rule-list">
          {routes.map((r, i) => (
            <div className="rule-row" key={i}>
              <input value={r.target} onChange={(e) => setRoute(i, "target", e.target.value)} placeholder="192.168.0.0/16 / example.com" />
              <select value={r.via === "direct" || r.via === "tunnel" ? r.via : "__nic__"} onChange={(e) => setRoute(i, "via", e.target.value === "__nic__" ? "" : e.target.value)}>
                <option value="direct">direct</option>
                <option value="tunnel">tunnel</option>
                <option value="__nic__">interface</option>
              </select>
              {r.via !== "direct" && r.via !== "tunnel" && <input value={r.via} onChange={(e) => setRoute(i, "via", e.target.value)} placeholder="eth1 / en0" />}
              <button className="btn icon-only danger" onClick={() => removeRoute(i)} aria-label="Remove rule"><IconTrash size={16} /></button>
            </div>
          ))}
          {routes.length === 0 && <EmptyState text={splitMode === "whitelist" ? "No rules. Nothing is tunnelled yet." : "No rules. Everything goes through the tunnel."} />}
        </div>
        <button className="btn ghost with-icon" onClick={addRoute}><IconPlus size={16} />Add rule</button>
      </div>
    </>
  );
}

function SettingsPage({ local, onChange }: { local: LocalState; onChange: () => void }) {
  const [s, setS] = useState<ConnectionSettings>(local.settings);
  const [saved, setSaved] = useState(false);
  const set = (k: keyof ConnectionSettings, v: unknown) => setS({ ...s, [k]: v });
  const save = async () => {
    await api.setSettings({
      ...local.settings,
      tun_name: s.tun_name,
      http_listen: s.http_listen,
      requested_ip: s.requested_ip,
      client_name: s.client_name,
      ipv6_killswitch: s.ipv6_killswitch,
    });
    setSaved(true);
    onChange();
    setTimeout(() => setSaved(false), 1200);
  };
  return (
    <>
      <PageTitle title="Settings" subtitle="Local runtime parameters">
        <button className="btn" onClick={save}>{saved ? "Saved" : "Save settings"}</button>
      </PageTitle>
      <div className="settings-grid">
        <div className="panel form-panel span-2">
          <PanelHead icon={<IconSettings size={19} />} title="Mode Parameters" hint="local only" />
          <div className="row two">
            <label className="field">TUN device<input value={s.tun_name} onChange={(e) => set("tun_name", e.target.value)} /></label>
            <label className="field">HTTP proxy listen<input value={s.http_listen} onChange={(e) => set("http_listen", e.target.value)} /></label>
          </div>
          <div className="row two">
            <label className="field">Requested virtual IP<input value={s.requested_ip ?? ""} onChange={(e) => set("requested_ip", e.target.value || null)} placeholder="server assigns by default" /></label>
            <label className="field">Device name<input value={s.client_name ?? ""} onChange={(e) => set("client_name", e.target.value || null)} placeholder="my-laptop" /></label>
          </div>
        </div>
        <div className="panel">
          <PanelHead icon={<IconShield size={19} />} title="IPv6 Protection" hint="kill-switch" />
          <div className="switch-row">
            <div><b>Enable IPv6 kill-switch</b><span>Prevent native IPv6 leaks on v4-only servers.</span></div>
            <label className="switch"><input type="checkbox" checked={s.ipv6_killswitch ?? true} onChange={(e) => set("ipv6_killswitch", e.target.checked)} /><span /></label>
          </div>
        </div>
      </div>
    </>
  );
}

function Logs() {
  const [lines, setLines] = useState<string[]>([]);
  const [follow, setFollow] = useState(true);
  const [filter, setFilter] = useState("");
  const boxRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    let alive = true;
    const pull = async () => {
      try { const l = await api.getLogs(); if (alive) setLines(l); } catch { /* outside Tauri */ }
    };
    pull();
    const t = setInterval(pull, 1000);
    return () => { alive = false; clearInterval(t); };
  }, []);
  const shown = filter ? lines.filter((l) => l.toLowerCase().includes(filter.toLowerCase())) : lines;
  useEffect(() => { if (follow && boxRef.current) boxRef.current.scrollTop = boxRef.current.scrollHeight; }, [shown, follow]);
  const copy = () => navigator.clipboard?.writeText(shown.join("\n")).catch(() => {});
  return (
    <>
      <PageTitle title="Logs" subtitle={`Live buffer (${shown.length})`}>
        <input className="log-filter" value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="filter" />
        <label className="check-inline"><input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} />Follow</label>
        <button className="btn ghost sm" onClick={copy} disabled={!shown.length}>Copy</button>
      </PageTitle>
      <div className="panel"><div className="logs" ref={boxRef}>{shown.length === 0 ? <EmptyState text="No logs yet. Connect a profile to see engine activity." /> : shown.map((l, i) => <div className="log-line" key={i}>{l}</div>)}</div></div>
    </>
  );
}

function PageTitle({ title, subtitle, children }: { title: string; subtitle: string; children?: ReactNode }) {
  return (
    <div className="page-title">
      <div><h2>{title}</h2><p>{subtitle}</p></div>
      <div className="title-actions">{children}</div>
    </div>
  );
}

function PanelHead({ icon, title, hint }: { icon: ReactNode; title: string; hint?: string }) {
  return <div className="panel-head"><span className="panel-icon">{icon}</span><b>{title}</b>{hint && <small>{hint}</small>}</div>;
}

function MetricCard({ label, value, hint, icon, tone }: { label: string; value: string; hint: string; icon: ReactNode; tone?: "up" | "down" }) {
  return <div className={"metric-card" + (tone ? ` ${tone}` : "")}><span>{icon}</span><div><small>{label}</small><b>{value}</b><em>{hint}</em></div></div>;
}

function MetricLine({ label, value }: { label: string; value: string }) {
  return <div className="metric-line"><span>{label}</span><b>{value}</b></div>;
}

function EmptyState({ text }: { text: string }) {
  return <div className="empty-state">{text}</div>;
}

function TrafficChart({ samples, id }: { samples: TrafficSample[]; id: string }) {
  const pathData = useMemo(() => makeTrafficPaths(samples, 720, 210), [samples]);
  if (samples.length < 2) return <div className="chart-empty">Waiting for traffic samples</div>;
  return (
    <svg className="traffic-chart" viewBox="0 0 720 210" preserveAspectRatio="none">
      <defs>
        <linearGradient id={`${id}-down`} x1="0" x2="0" y1="0" y2="1"><stop offset="0" stopColor="#3f8cff" stopOpacity="0.35" /><stop offset="1" stopColor="#3f8cff" stopOpacity="0" /></linearGradient>
        <linearGradient id={`${id}-up`} x1="0" x2="0" y1="0" y2="1"><stop offset="0" stopColor="#f5a524" stopOpacity="0.26" /><stop offset="1" stopColor="#f5a524" stopOpacity="0" /></linearGradient>
      </defs>
      <path d={pathData.downArea} fill={`url(#${id}-down)`} />
      <path d={pathData.upArea} fill={`url(#${id}-up)`} />
      <path d={pathData.downLine} className="chart-line down" />
      <path d={pathData.upLine} className="chart-line up" />
    </svg>
  );
}

function MiniTrafficChart({ samples }: { samples: TrafficSample[] }) {
  const paths = useMemo(() => makeTrafficPaths(samples, 180, 48), [samples]);
  if (samples.length < 2) return <div className="mini-chart empty" />;
  return <svg className="mini-chart" viewBox="0 0 180 48" preserveAspectRatio="none"><path d={paths.downLine} className="chart-line down" /><path d={paths.upLine} className="chart-line up" /></svg>;
}

function makeTrafficPaths(samples: TrafficSample[], w: number, h: number) {
  const pad = 8;
  const maxV = Math.max(1, ...samples.map((s) => Math.max(s.upRate, s.downRate)));
  const n = Math.max(2, samples.length);
  const x = (i: number) => pad + (i / (n - 1)) * (w - pad * 2);
  const y = (v: number) => h - pad - (v / maxV) * (h - pad * 2);
  const line = (pick: (s: TrafficSample) => number) => "M " + samples.map((s, i) => `${x(i).toFixed(1)},${y(pick(s)).toFixed(1)}`).join(" L ");
  const area = (pick: (s: TrafficSample) => number) => {
    const pts = samples.map((s, i) => `${x(i).toFixed(1)},${y(pick(s)).toFixed(1)}`).join(" L ");
    return `M ${x(0).toFixed(1)},${(h - pad).toFixed(1)} L ${pts} L ${x(samples.length - 1).toFixed(1)},${(h - pad).toFixed(1)} Z`;
  };
  return { upLine: line((s) => s.upRate), downLine: line((s) => s.downRate), upArea: area((s) => s.upRate), downArea: area((s) => s.downRate) };
}
