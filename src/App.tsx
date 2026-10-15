import * as Dialog from "@radix-ui/react-dialog";
import * as Switch from "@radix-ui/react-switch";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import { inTauri, launcherApi } from "./api";
import type {
  AgentLogLine,
  AgentRelease,
  AgentStatus,
  Bootstrap,
  GameStatus,
  GameView,
  LauncherUpdate,
  NewsItem,
  PatchProgress,
  ServerEntry,
  Settings,
} from "./types";

const api = launcherApi();

function versionLabel(game: GameView, status?: GameStatus): string {
  if (!status || status.state === "checking") return "…";
  if (game.kind === "web") {
    return status.webBuild ? `Build ${status.webBuild}` : "Web";
  }
  return status.installedVersion || status.remoteVersion || "—";
}

function playControl(game: GameView, status: GameStatus | undefined, patching: boolean) {
  if (patching) return { label: "PATCHING", disabled: true };
  if (!status || status.state === "checking") return { label: "CHECKING", disabled: true };
  if (game.kind === "web") return { label: "PLAY", disabled: false };
  if (status.state === "ready") return { label: "PLAY", disabled: false };
  if (status.state === "update-available") return { label: "UPDATE", disabled: false };
  if (status.state === "not-installed") return { label: "INSTALL", disabled: false };
  if (status.state === "check-failed" && status.installedVersion) return { label: "PLAY", disabled: false };
  if (status.state === "check-failed") return { label: "RETRY", disabled: false };
  return { label: "PLAY", disabled: true };
}

export default function App() {
  const [boot, setBoot] = useState<Bootstrap | null>(null);
  const [bootError, setBootError] = useState("");
  const [selectedId, setSelectedId] = useState("friday-night-fights");
  const [statuses, setStatuses] = useState<Record<string, GameStatus>>({});
  const [progress, setProgress] = useState<PatchProgress | null>(null);
  const [patchingId, setPatchingId] = useState<string | null>(null);
  const [serverId, setServerId] = useState("dolmar");
  const [probe, setProbe] = useState("unknown");
  const [world, setWorld] = useState<{ state: string; message: string } | null>(null);
  const [selfUpdating, setSelfUpdating] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [notesOpen, setNotesOpen] = useState(false);
  const [newsOpen, setNewsOpen] = useState(false);
  const [adminOpen, setAdminOpen] = useState(false);
  const [adminUnlocked, setAdminUnlocked] = useState(false);
  const [news, setNews] = useState<NewsItem[]>([]);
  const [launcherUpdate, setLauncherUpdate] = useState<LauncherUpdate | null>(null);
  const [actionError, setActionError] = useState("");
  const patchingRef = useRef<string | null>(null);

  const refreshStatus = useCallback(async (game: GameView, remote: boolean) => {
    setStatuses((prev) => ({
      ...prev,
      [game.id]: prev[game.id] ?? {
        gameId: game.id,
        state: "checking",
        patchNotes: "",
        message: "",
      },
    }));
    try {
      const status = await api.inspect(game.id, remote);
      setStatuses((prev) => ({ ...prev, [game.id]: status }));
      return status;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      const failed: GameStatus = {
        gameId: game.id,
        state: "check-failed",
        patchNotes: "",
        message,
      };
      setStatuses((prev) => ({ ...prev, [game.id]: failed }));
      return failed;
    }
  }, []);

  useEffect(() => {
    let stop = false;
    api
      .bootstrap()
      .then((data) => {
        if (stop) return;
        setBoot(data);
        setNews(data.news);
        const saved = data.settings.selectedServer["friday-night-fights"];
        if (saved) setServerId(saved);
      })
      .catch((err: unknown) => setBootError(err instanceof Error ? err.message : String(err)));
    return () => {
      stop = true;
    };
  }, []);

  useEffect(() => {
    if (!boot?.hasAgentSecret) return;
    const server = boot.games.flatMap((game) => game.servers).find((entry) => entry.agentUrl);
    if (!server) return;
    let stop = false;
    api.adminProbe(server.id).then(
      () => {
        if (!stop) setAdminUnlocked(true);
      },
      () => {
        if (!stop) setAdminUnlocked(false);
      },
    );
    return () => {
      stop = true;
    };
  }, [boot?.hasAgentSecret]);

  useEffect(() => {
    if (!boot) return;
    let unlisten: () => void = () => {};
    api.onPatch((event) => {
      setProgress(event);
      if (event.phase === "done" || event.phase === "error") {
        setPatchingId((current) => (current === event.gameId ? null : current));
      }
    }).then((off) => {
      unlisten = off;
    });
    const remote = boot.settings.checkOnStart;
    for (const game of boot.games) {
      void refreshStatus(game, remote).then((status) => {
        if (
          remote &&
          game.kind === "unreal-client" &&
          status.state === "update-available" &&
          status.installedVersion
        ) {
          void beginPatch(game.id);
        }
      });
    }
    if (remote) {
      api.checkLauncherUpdate().then(setLauncherUpdate).catch(() => setLauncherUpdate(null));
    }
    return () => unlisten();
    // beginPatch is stable enough for startup; including it retriggers the check.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [boot, refreshStatus]);

  const game = boot?.games.find((item) => item.id === selectedId) ?? boot?.games[0];
  const status = game ? statuses[game.id] : undefined;
  const patching = Boolean(game && patchingId === game.id);
  const control = game ? playControl(game, status, patching) : { label: "CHECKING", disabled: true };

  const selectedServer = useMemo(() => {
    if (!game) return undefined;
    return game.servers.find((server) => server.id === serverId) ?? game.servers.find((server) => server.default) ?? game.servers[0];
  }, [game, serverId]);

  useEffect(() => {
    if (!selectedServer || game?.kind === "web") return;
    let cancel = false;
    setProbe("unknown");
    api.probe(selectedServer.host, selectedServer.port).then((value) => {
      if (!cancel) setProbe(value);
    });
    return () => {
      cancel = true;
    };
  }, [selectedServer, game?.kind]);

  useEffect(() => {
    if (!selectedServer || game?.kind === "web") return;
    let cancel = false;
    const poll = () =>
      api.worldStatus(selectedServer.id).then((value) => !cancel && setWorld(value)).catch(() => !cancel && setWorld(null));
    void poll();
    const timer = window.setInterval(poll, 20000);
    return () => {
      cancel = true;
      window.clearInterval(timer);
    };
  }, [selectedServer, game?.kind]);

  async function beginPatch(gameId: string) {
    if (patchingRef.current) return;
    patchingRef.current = gameId;
    setActionError("");
    setPatchingId(gameId);
    setProgress({ gameId, phase: "check", received: 0, total: 0, percent: 0, message: "Checking release" });
    try {
      const next = await api.startPatch(gameId);
      setStatuses((prev) => ({ ...prev, [gameId]: next }));
      setProgress(null);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setActionError(message);
      setProgress({ gameId, phase: "error", received: 0, total: 0, percent: 0, message });
    } finally {
      patchingRef.current = null;
      setPatchingId((current) => (current === gameId ? null : current));
    }
  }

  async function onPlay() {
    if (!game) return;
    setActionError("");
    if (control.label === "INSTALL" || control.label === "UPDATE") {
      await beginPatch(game.id);
      return;
    }
    if (control.label === "RETRY") {
      const remote = true;
      const next = await refreshStatus(game, remote);
      if (next.state === "update-available" || next.state === "not-installed") {
        await beginPatch(game.id);
      }
      return;
    }
    try {
      await api.play(game.id, selectedServer?.id ?? null);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    }
  }

  async function onServer(id: string) {
    setServerId(id);
    if (!boot || !game) return;
    const settings: Settings = {
      ...boot.settings,
      selectedServer: { ...boot.settings.selectedServer, [game.id]: id },
    };
    setBoot({ ...boot, settings });
    try {
      await api.saveSettings(settings);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    }
  }

  if (bootError) {
    return (
      <main className="shell">
        <p className="error-line">The launcher could not start. {bootError}</p>
      </main>
    );
  }
  if (!boot || !game) {
    return (
      <main className="shell">
        <p className="muted">Opening the guild library…</p>
      </main>
    );
  }

  const showBar = patching || progress?.gameId === game.id;
  const percent = progress && progress.gameId === game.id ? progress.percent : 0;
  const statusText = actionError
    || (progress && progress.gameId === game.id ? progress.message : "")
    || status?.message
    || (status?.state === "ready" && game.kind !== "web" ? `Up to date${status.installedVersion ? ` · ${status.installedVersion}` : ""}` : "")
    || (status?.state === "update-available" ? `Update available · ${status.remoteVersion ?? ""}` : "")
    || (status?.state === "not-installed" ? `Not installed${status.remoteVersion ? ` · ${status.remoteVersion} is available` : ""}` : "")
    || (status?.state === "checking" ? "Checking for updates…" : "")
    || (game.kind === "web" ? "Opens in your browser" : "");

  const notesTitle = game.kind === "web"
    ? (status?.patchNotes || "Build")
    : (status?.releaseName || status?.remoteVersion || game.name);
  const heroVersion = game.kind === "web"
    ? (status?.webBuild ? `Build ${status.webBuild}` : "Build")
    : (status?.installedVersion || status?.remoteVersion || "");

  return (
    <div className="shell">
      <Noise />
      <aside className="library">
        <div className="brand">
          <img className="crest" src="/art/uxa-crest.jpg" alt="uXa crest" />
          <h1>uXa Launcher</h1>
        </div>
        <div className="section-label">Game Library</div>
        <div className="game-list">
          {boot.games.map((item) => (
            <button
              key={item.id}
              type="button"
              className={item.id === game.id ? "game-card selected" : "game-card"}
              onClick={() => {
                setSelectedId(item.id);
                setActionError("");
                const saved = boot.settings.selectedServer[item.id];
                const fallback = item.servers.find((server) => server.default)?.id ?? item.servers[0]?.id;
                if (saved || fallback) setServerId(saved || fallback || "");
              }}
            >
              <img src={item.thumbnail} alt="" />
              <span>
                <strong>{item.name}</strong>
                <span>{versionLabel(item, statuses[item.id])}</span>
              </span>
            </button>
          ))}
          <div className="more">
            <span className="hourglass" aria-hidden="true">⏳</span>
            <span>More coming</span>
            <span className="lock" aria-hidden="true">🔒</span>
          </div>
        </div>
        <div className="library-foot">
          <span className="foot-actions">
            <button type="button" className="gear" onClick={() => setSettingsOpen(true)}>
              <Gear /> Settings
            </button>
            {adminUnlocked && (
              <button type="button" className="gear" onClick={() => setAdminOpen(true)}>
                <Shield /> Admin
              </button>
            )}
          </span>
          <span>Launcher {boot.displayVersion}</span>
        </div>
      </aside>

      <main className="center">
        {launcherUpdate && (
          <div className="update-banner">
            <span>{selfUpdating ?? `Launcher ${launcherUpdate.latest} is available.`}</span>
            {launcherUpdate.asset && (
              <button
                type="button"
                disabled={!!selfUpdating}
                onClick={() => {
                  setSelfUpdating("Updating the launcher…");
                  api.applyLauncherUpdate().catch((err) => setSelfUpdating(`Update failed: ${String(err)}`));
                }}
              >
                Update now
              </button>
            )}
            {launcherUpdate.url && (
              <button type="button" onClick={() => void api.openUrl(launcherUpdate.url)}>
                View release
              </button>
            )}
          </div>
        )}
        {world && game.kind !== "web" && (
          <div className={`world-status world-${world.state}`} role="status">
            <span className="world-dot" /> {world.message}
          </div>
        )}
        <section className="hero" aria-label={`${game.name} banner`}>
          <img src={game.hero || game.thumbnail} alt="" />
          <div className="shade" />
          <div className="hero-copy">
            <h2>{game.name}</h2>
            {heroVersion && <p>{heroVersion}{game.kind === "web" && status?.webUpdated ? ` · ${status.webUpdated}` : ""}</p>}
          </div>
        </section>

        <button
          type="button"
          className={control.label.length > 5 ? "play play-long" : "play"}
          disabled={control.disabled}
          onClick={() => void onPlay()}
        >
          {control.label}
          <span className="diamond" aria-hidden="true" />
        </button>

        {game.kind === "web" ? (
          <p className="hint">Play opens {status?.webUrl || "the game page"} in your browser. Nothing is downloaded.</p>
        ) : (
          <div className="server-row">
            <label className="server">
              <Shield />
              <span
                className={`dot ${probe}`}
                title="Best-effort TCP probe. The game uses UDP, so unknown does not mean the server is down."
              />
              <select
                aria-label="Server"
                value={selectedServer?.id ?? ""}
                onChange={(event) => void onServer(event.target.value)}
              >
                {game.servers.map((server) => (
                  <option key={server.id} value={server.id}>{server.name}</option>
                ))}
              </select>
              <span className="chev" aria-hidden="true">▾</span>
            </label>
            {selectedServer && (
              <span className="addr">{selectedServer.host}:{selectedServer.port}</span>
            )}
          </div>
        )}

        {game.kind !== "web" && (
          <div className="patch">
            <div className="patch-meta">
              <span className={actionError || progress?.phase === "error" ? "bad" : status?.state === "check-failed" ? "warn" : ""}>
                {patching ? "Patching…" : statusText || " "}
              </span>
              {showBar && progress && progress.total > 0 && <span>{percent}%</span>}
            </div>
            {showBar && (
              <div
                className={progress && progress.total > 0 ? "bar" : "bar indeterminate"}
                role="progressbar"
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={percent}
              >
                <span style={{ width: progress && progress.total > 0 ? `${percent}%` : undefined }} />
              </div>
            )}
          </div>
        )}
        {!inTauri() && (
          <p className="muted">This window is the interface preview. Download, install, and launch run in the desktop app.</p>
        )}
      </main>

      <aside className="rail">
        <section className="panel notes">
          <div className="panel-head"><Diamond /> PATCH NOTES</div>
          <h3>
            {notesTitle}
            {status?.remoteVersion && game.kind !== "web" && <span className="badge">Latest</span>}
          </h3>
          <div className="md">
            {status?.patchNotes ? <ReactMarkdown>{status.patchNotes}</ReactMarkdown> : <p className="muted">Patch notes show up here after a release check.</p>}
          </div>
          {status?.patchNotes && (
            <button type="button" className="text-link" onClick={() => setNotesOpen(true)}>View full patch notes →</button>
          )}
        </section>
        <section className="panel news">
          <div className="panel-head"><Shield /> GUILD NEWS</div>
          <NewsList items={news} />
          <button type="button" className="text-link" onClick={() => setNewsOpen(true)}>View all news →</button>
        </section>
      </aside>

      <SettingsDialog
        open={settingsOpen}
        boot={boot}
        adminUnlocked={adminUnlocked}
        onOpenChange={setSettingsOpen}
        onSaved={(next, hasToken) => setBoot({ ...boot, settings: next, hasGithubToken: hasToken })}
        onNews={setNews}
        onAdmin={(unlocked, remembered) => {
          setAdminUnlocked(unlocked);
          setBoot((current) => current ? { ...current, hasAgentSecret: remembered } : current);
          if (unlocked) setAdminOpen(true);
        }}
      />
      <AdminDialog open={adminOpen} boot={boot} onOpenChange={setAdminOpen} />
      <MarkdownDialog open={notesOpen} onOpenChange={setNotesOpen} title={notesTitle} body={status?.patchNotes ?? ""} />
      <Dialog.Root open={newsOpen} onOpenChange={setNewsOpen}>
        <Dialog.Portal>
          <Dialog.Overlay className="overlay" />
          <Dialog.Content className="dialog">
            <Dialog.Title asChild><h2>Guild news</h2></Dialog.Title>
            <NewsList items={news} />
            <div className="dialog-actions">
              <Dialog.Close className="btn">Close</Dialog.Close>
            </div>
          </Dialog.Content>
        </Dialog.Portal>
      </Dialog.Root>
    </div>
  );
}

function adminServers(boot: Bootstrap): Array<ServerEntry & { game: string }> {
  return boot.games.flatMap((game) =>
    game.servers.filter((server) => server.agentUrl).map((server) => ({ ...server, game: game.name })),
  );
}

function formatUptime(seconds: number): string {
  const hrs = Math.floor(seconds / 3600);
  const mins = Math.floor((seconds % 3600) / 60);
  const secs = seconds % 60;
  if (hrs > 0) return `${hrs}h ${mins}m`;
  if (mins > 0) return `${mins}m ${secs}s`;
  return `${secs}s`;
}

function AdminDialog({
  open,
  boot,
  onOpenChange,
}: {
  open: boolean;
  boot: Bootstrap;
  onOpenChange: (open: boolean) => void;
}) {
  const servers = adminServers(boot);
  const [serverId, setServerId] = useState(servers[0]?.id ?? "");
  const [releases, setReleases] = useState<AgentRelease[]>([]);
  const [tag, setTag] = useState("");
  const [status, setStatus] = useState<AgentStatus | null>(null);
  const [logs, setLogs] = useState<AgentLogLine[]>([]);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [confirm, setConfirm] = useState<null | "deploy" | "restart" | "stop">(null);
  const after = useRef(0);

  const server = servers.find((entry) => entry.id === serverId) ?? servers[0];
  const activeId = server?.id ?? "";

  const refresh = useCallback(async () => {
    if (!activeId) return;
    try {
      const next = await api.adminProbe(activeId);
      setStatus(next);
      const lines = await api.adminLogs(activeId, after.current);
      if (lines.length > 0) {
        after.current = lines[lines.length - 1].id;
        setLogs((prev) => [...prev, ...lines].slice(-200));
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, [activeId]);

  useEffect(() => {
    if (!open || !activeId) return;
    let stop = false;
    setLogs([]);
    after.current = 0;
    setError("");
    api.adminReleases(activeId).then(
      (list) => {
        if (stop) return;
        setReleases(list);
        setTag((current) => current || list[0]?.tag || "");
      },
      (err: unknown) => {
        if (!stop) setError(err instanceof Error ? err.message : String(err));
      },
    );
    void refresh();
    const timer = window.setInterval(() => void refresh(), 2000);
    return () => {
      stop = true;
      window.clearInterval(timer);
    };
  }, [open, activeId, refresh]);

  async function run(action: "deploy" | "restart" | "stop") {
    if (!server) return;
    setConfirm(null);
    setBusy(true);
    setError("");
    setNotice("");
    try {
      const result = action === "deploy"
        ? await api.adminDeploy(server.id, tag)
        : action === "restart"
          ? await api.adminRestart(server.id)
          : await api.adminStop(server.id);
      setStatus(result.status);
      setNotice(result.message);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      await refresh();
    } finally {
      setBusy(false);
    }
  }

  const confirmCopy = confirm === "deploy"
    ? `Deploy ${tag || "the selected build"} to ${server?.name ?? "this server"}? Players get a warning, world saves are backed up, then the server restarts. If the new build does not stay up, the previous build is restored.`
    : confirm === "restart"
      ? `Restart ${server?.name ?? "this server"} on the current build? Players get a warning first.`
      : `Stop ${server?.name ?? "this server"}? Players get a warning, then the process exits.`;

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className="dialog wide">
          <Dialog.Title asChild><h2>Server admin</h2></Dialog.Title>
          <Dialog.Description className="muted">
            Deploy a Friday Night Fights dedicated server. The agent on that machine checks the admin key on every request.
          </Dialog.Description>
          {servers.length === 0 ? (
            <p className="error-line">No server has an agent address. Add agentUrl in servers.json.</p>
          ) : (
            <>
              <label className="field">
                <span>Server</span>
                <select aria-label="Admin server" value={server?.id ?? ""} onChange={(event) => {
                  setServerId(event.target.value);
                  setTag("");
                  setLogs([]);
                  after.current = 0;
                }}>
                  {servers.map((entry) => (
                    <option key={entry.id} value={entry.id}>{entry.game} — {entry.name}</option>
                  ))}
                </select>
                {server && <small>{server.agentUrl}</small>}
              </label>
              <label className="field">
                <span>Release</span>
                <select aria-label="Server release" value={tag} onChange={(event) => setTag(event.target.value)}>
                  {releases.length === 0 && <option value="">No matching releases</option>}
                  {releases.map((release) => (
                    <option key={release.tag} value={release.tag}>
                      {release.tag}{release.prerelease ? " (pre-release)" : ""} — {release.assetName}
                    </option>
                  ))}
                </select>
              </label>
              <div className="status-line">
                <span>Version {status?.version || "—"}</span>
                <span className={status?.online ? "ok" : ""}>{status?.online ? "Online" : "Offline"}</span>
                <span>Uptime {status?.online ? formatUptime(status.uptimeSecs) : "—"}</span>
                <span>Players {status?.playerCount ?? "—"}</span>
                <span>{status?.phase || "—"}</span>
              </div>
              <div className="row-actions">
                <button type="button" className="btn primary" disabled={busy || !tag} onClick={() => setConfirm("deploy")}>
                  Deploy &amp; Restart
                </button>
                <button type="button" className="btn" disabled={busy} onClick={() => setConfirm("restart")}>
                  Restart only
                </button>
                <button type="button" className="btn danger" disabled={busy} onClick={() => setConfirm("stop")}>
                  Stop
                </button>
              </div>
              <pre className="admin-log" aria-live="polite">
                {logs.length === 0 ? "Waiting for agent log…" : logs.map((line) => `${line.at}  ${line.message}`).join("\n")}
              </pre>
            </>
          )}
          {notice && <p className="token-status">{notice}</p>}
          {error && <p className="error-line">{error}</p>}
          <div className="dialog-actions">
            <Dialog.Close className="btn">Close</Dialog.Close>
          </div>
          <Dialog.Root open={confirm !== null} onOpenChange={(next) => { if (!next) setConfirm(null); }}>
            <Dialog.Portal>
              <Dialog.Overlay className="overlay" />
              <Dialog.Content className="dialog">
                <Dialog.Title asChild><h2>Confirm</h2></Dialog.Title>
                <Dialog.Description>{confirmCopy}</Dialog.Description>
                <div className="dialog-actions">
                  <button type="button" className="btn" onClick={() => setConfirm(null)}>Cancel</button>
                  <button
                    type="button"
                    className={confirm === "stop" ? "btn danger" : "btn primary"}
                    onClick={() => { if (confirm) void run(confirm); }}
                  >
                    {confirm === "deploy" ? "Deploy" : confirm === "restart" ? "Restart" : "Stop"}
                  </button>
                </div>
              </Dialog.Content>
            </Dialog.Portal>
          </Dialog.Root>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function NewsList({ items }: { items: NewsItem[] }) {
  if (items.length === 0) {
    return <p className="muted">No guild news yet. A news.json address can be set in Settings.</p>;
  }
  return (
    <ul className="news-list">
      {items.map((item) => (
        <li key={item.id}>
          <span className="guild-mark" aria-hidden="true"><Shield /></span>
          <div>
            <strong>{item.guild}</strong>
            <p>{item.body}</p>
          </div>
          <span className="ago">{item.ago}</span>
        </li>
      ))}
    </ul>
  );
}

function SettingsDialog({
  open,
  boot,
  adminUnlocked,
  onOpenChange,
  onSaved,
  onNews,
  onAdmin,
}: {
  open: boolean;
  boot: Bootstrap;
  adminUnlocked: boolean;
  onOpenChange: (open: boolean) => void;
  onSaved: (settings: Settings, hasToken: boolean) => void;
  onNews: (items: NewsItem[]) => void;
  onAdmin: (unlocked: boolean, remembered: boolean) => void;
}) {
  const [draft, setDraft] = useState(boot.settings);
  const [token, setToken] = useState("");
  const [hasToken, setHasToken] = useState(boot.hasGithubToken);
  const [message, setMessage] = useState("");
  const [error, setError] = useState("");
  const [adminKey, setAdminKey] = useState("");
  const [rememberAdmin, setRememberAdmin] = useState(false);
  const [adminMessage, setAdminMessage] = useState("");

  useEffect(() => {
    if (open) {
      setDraft(boot.settings);
      setHasToken(boot.hasGithubToken);
      setToken("");
      setMessage("");
      setError("");
      setAdminKey("");
      setAdminMessage("");
    }
  }, [open, boot]);

  const fnfOverride = draft.installOverrides["friday-night-fights"] ?? "";

  async function save() {
    setError("");
    try {
      await api.saveSettings(draft);
      if (token.trim()) {
        await api.setToken(token.trim());
        setHasToken(true);
        setToken("");
      }
      const items = await api.refreshNews();
      onNews(items);
      onSaved(draft, token.trim() ? true : hasToken);
      setMessage("Settings saved.");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className="dialog">
          <Dialog.Title asChild><h2>Settings</h2></Dialog.Title>
          <Dialog.Description className="muted">
            Install folders, the optional GitHub token, and the guild news feed. Launcher {boot.displayVersion} ({boot.launcherVersion}).
          </Dialog.Description>
          <label className="field">
            <span>Games folder</span>
            <input
              value={draft.installRoot}
              placeholder={boot.defaultInstallRoot}
              onChange={(event) => setDraft({ ...draft, installRoot: event.target.value })}
            />
            <small>Friday Night Fights is installed in a FridayNightFights folder inside this path.</small>
          </label>
          <label className="field">
            <span>Friday Night Fights install folder</span>
            <input
              value={fnfOverride}
              placeholder="Full path, if it should not use the games folder"
              onChange={(event) => {
                const installOverrides = { ...draft.installOverrides };
                if (event.target.value.trim()) installOverrides["friday-night-fights"] = event.target.value;
                else delete installOverrides["friday-night-fights"];
                setDraft({ ...draft, installOverrides });
              }}
            />
            <small>Leave blank to use the games folder. Saves in Saved/SaveGames are kept across updates.</small>
          </label>
          <label className="switch-row">
            <span>Check for updates on start</span>
            <Switch.Root
              className="switch"
              checked={draft.checkOnStart}
              onCheckedChange={(checked) => setDraft({ ...draft, checkOnStart: checked })}
            >
              <Switch.Thumb />
            </Switch.Root>
          </label>
          <label className="field">
            <span>GitHub token</span>
            <input
              type="password"
              autoComplete="off"
              value={token}
              placeholder={hasToken ? "Token saved — enter a new one to replace it" : "Optional, for private releases"}
              onChange={(event) => setToken(event.target.value)}
            />
            <small className="token-status">{hasToken ? "A token is stored for this user." : "No token stored. Public releases work without one."}</small>
          </label>
          <div className="row-actions">
            <button
              type="button"
              className="btn"
              onClick={() => {
                void api.clearToken().then(() => {
                  setHasToken(false);
                  setToken("");
                  onSaved(draft, false);
                  setMessage("Token removed.");
                }).catch((err: unknown) => setError(err instanceof Error ? err.message : String(err)));
              }}
            >
              Clear token
            </button>
          </div>
          <label className="field">
            <span>Guild news URL</span>
            <input
              value={draft.newsUrl}
              placeholder="https://example.com/news.json"
              onChange={(event) => setDraft({ ...draft, newsUrl: event.target.value })}
            />
            <small>JSON object with an items array: id, guild, ago, body. Blank uses the bundled placeholder.</small>
          </label>
          <label className="field">
            <span>Launcher update repository</span>
            <input
              value={draft.launcherUpdateOwner}
              placeholder="GitHub owner"
              onChange={(event) => setDraft({ ...draft, launcherUpdateOwner: event.target.value })}
            />
            <input
              value={draft.launcherUpdateRepo}
              placeholder="GitHub repo"
              onChange={(event) => setDraft({ ...draft, launcherUpdateRepo: event.target.value })}
            />
            <small>Optional. A newer release is reported here; the launcher does not install it.</small>
          </label>
          <div className="admin-key">
            <label className="field">
              <span>Admin key</span>
              <input
                type="password"
                autoComplete="off"
                value={adminKey}
                placeholder={boot.hasAgentSecret || adminUnlocked ? "Key saved — enter it again to replace" : "Server agent shared key"}
                onChange={(event) => setAdminKey(event.target.value)}
              />
              <small>Unlocks server deploy for this session. It is not written into settings.</small>
            </label>
            <label className="switch-row">
              <span>Remember admin key on this PC</span>
              <Switch.Root className="switch" checked={rememberAdmin} onCheckedChange={setRememberAdmin}>
                <Switch.Thumb />
              </Switch.Root>
            </label>
            <div className="row-actions">
              <button
                type="button"
                className="btn"
                onClick={() => {
                  const server = boot.games.flatMap((game) => game.servers).find((entry) => entry.agentUrl);
                  if (!server) {
                    setError("No server has an agent address.");
                    return;
                  }
                  setError("");
                  void api.unlockAdmin(adminKey, server.id, rememberAdmin).then(
                    () => {
                      setAdminKey("");
                      setAdminMessage("Admin key accepted.");
                      onAdmin(true, rememberAdmin);
                    },
                    (err: unknown) => setError(err instanceof Error ? err.message : String(err)),
                  );
                }}
              >
                Unlock admin
              </button>
              {(boot.hasAgentSecret || adminUnlocked) && (
                <button
                  type="button"
                  className="btn"
                  onClick={() => {
                    void api.forgetAdmin().then(
                      () => {
                        setAdminMessage("Admin key removed.");
                        onAdmin(false, false);
                      },
                      (err: unknown) => setError(err instanceof Error ? err.message : String(err)),
                    );
                  }}
                >
                  Forget admin key
                </button>
              )}
            </div>
            {adminMessage && <p className="token-status">{adminMessage}</p>}
          </div>
          {message && <p className="token-status">{message}</p>}
          {error && <p className="error-line">{error}</p>}
          <div className="dialog-actions">
            <button type="button" className="btn" onClick={() => void api.openLogs().catch((err: unknown) => setError(String(err)))}>
              Open logs
            </button>
            <button type="button" className="btn primary" onClick={() => void save()}>Save</button>
            <Dialog.Close className="btn">Close</Dialog.Close>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function MarkdownDialog({
  open,
  onOpenChange,
  title,
  body,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  title: string;
  body: string;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Overlay className="overlay" />
        <Dialog.Content className="dialog wide">
          <Dialog.Title asChild><h2>{title}</h2></Dialog.Title>
          <div className="md"><ReactMarkdown>{body}</ReactMarkdown></div>
          <div className="dialog-actions">
            <Dialog.Close className="btn">Close</Dialog.Close>
          </div>
        </Dialog.Content>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

function Noise() {
  return (
    <svg className="noise" aria-hidden="true">
      <filter id="stone">
        <feTurbulence type="fractalNoise" baseFrequency="0.8" numOctaves="4" stitchTiles="stitch" />
        <feColorMatrix type="saturate" values="0" />
      </filter>
      <rect width="100%" height="100%" filter="url(#stone)" />
    </svg>
  );
}

function Shield() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <path d="M12 3 5 6v6c0 4.2 2.8 7.2 7 9 4.2-1.8 7-4.8 7-9V6l-7-3Z" stroke="#8fce7a" strokeWidth="1.6" />
    </svg>
  );
}

function Diamond() {
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <rect x="6" y="1" width="7" height="7" transform="rotate(45 6 1)" fill="#8fce7a" />
    </svg>
  );
}

function Gear() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" aria-hidden="true">
      <circle cx="12" cy="12" r="3" stroke="currentColor" strokeWidth="1.6" />
      <path d="M12 3.5v2.2M12 18.3v2.2M3.5 12h2.2M18.3 12h2.2M6 6l1.6 1.6M16.4 16.4 18 18M18 6l-1.6 1.6M7.6 16.4 6 18" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
    </svg>
  );
}
