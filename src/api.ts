import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AgentAction,
  AgentLogLine,
  AgentRelease,
  AgentStatus,
  Bootstrap,
  GameStatus,
  LauncherUpdateOutcome,
  LauncherUpdateProgress,
  NewsItem,
  PatchProgress,
  PlayResult,
  Settings,
} from "./types";

export function inTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export interface LauncherApi {
  bootstrap(): Promise<Bootstrap>;
  inspect(gameId: string, remote: boolean): Promise<GameStatus>;
  startPatch(gameId: string): Promise<GameStatus>;
  onPatch(handler: (progress: PatchProgress) => void): Promise<() => void>;
  play(gameId: string, serverId: string | null): Promise<PlayResult>;
  saveSettings(settings: Settings): Promise<void>;
  setToken(token: string): Promise<void>;
  clearToken(): Promise<void>;
  probe(host: string, port: number): Promise<string>;
  applyLauncherUpdate(): Promise<LauncherUpdateOutcome>;
  onLauncherUpdate(handler: (progress: LauncherUpdateProgress) => void): Promise<() => void>;
  openLogs(): Promise<void>;
  openUrl(url: string): Promise<void>;
  refreshNews(): Promise<NewsItem[]>;
  unlockAdmin(secret: string, serverId: string, remember: boolean): Promise<AgentStatus>;
  adminProbe(serverId: string): Promise<AgentStatus>;
  forgetAdmin(): Promise<void>;
  adminReleases(serverId: string): Promise<AgentRelease[]>;
  adminLogs(serverId: string, after: number): Promise<AgentLogLine[]>;
  adminDeploy(serverId: string, tag: string): Promise<AgentAction>;
  adminRestart(serverId: string): Promise<AgentAction>;
  adminStop(serverId: string): Promise<AgentAction>;
}

const tauriApi: LauncherApi = {
  bootstrap: () => invoke<Bootstrap>("bootstrap"),
  inspect: (gameId, remote) => invoke<GameStatus>("inspect_game", { gameId, remote }),
  startPatch: (gameId) => invoke<GameStatus>("start_patch", { gameId }),
  async onPatch(handler) {
    const unlisten: UnlistenFn = await listen<PatchProgress>("patch-progress", (event) => {
      handler(event.payload);
    });
    return unlisten;
  },
  play: (gameId, serverId) => invoke<PlayResult>("play", { gameId, serverId }),
  saveSettings: (settings) => invoke("save_settings", { settings }),
  setToken: (token) => invoke("set_github_token", { token }),
  clearToken: () => invoke("clear_github_token"),
  probe: (host, port) => invoke<string>("probe_server", { host, port }),
  applyLauncherUpdate: () => invoke<LauncherUpdateOutcome>("apply_launcher_update"),
  async onLauncherUpdate(handler) {
    const unlisten: UnlistenFn = await listen<LauncherUpdateProgress>("launcher-update", (event) => {
      handler(event.payload);
    });
    return unlisten;
  },
  openLogs: () => invoke("open_logs"),
  openUrl: (url) => invoke("open_url", { url }),
  refreshNews: () => invoke<NewsItem[]>("refresh_news"),
  unlockAdmin: (secret, serverId, remember) =>
    invoke<AgentStatus>("unlock_admin", { secret, serverId, remember }),
  adminProbe: (serverId) => invoke<AgentStatus>("admin_probe", { serverId }),
  forgetAdmin: () => invoke("forget_admin"),
  adminReleases: (serverId) => invoke<AgentRelease[]>("admin_releases", { serverId }),
  adminLogs: (serverId, after) => invoke<AgentLogLine[]>("admin_logs", { serverId, after }),
  adminDeploy: (serverId, tag) => invoke<AgentAction>("admin_deploy", { serverId, tag }),
  adminRestart: (serverId) => invoke<AgentAction>("admin_restart", { serverId }),
  adminStop: (serverId) => invoke<AgentAction>("admin_stop", { serverId }),
};

const previewGames: Bootstrap["games"] = [
  {
    id: "friday-night-fights",
    name: "Friday Night Fights",
    kind: "unreal-client",
    thumbnail: "/art/fnf-thumb.jpg",
    hero: "/art/fnf-hero.jpg",
    servers: [
      { id: "dolmar", name: "Dolmar's Server", host: "71.71.45.128", port: 7777, default: true, agentUrl: "https://71.71.45.128:8787" },
      { id: "localhost", name: "Localhost", host: "127.0.0.1", port: 7777 },
    ],
  },
  {
    id: "grave-march",
    name: "Grave March",
    kind: "web",
    thumbnail: "/art/grave-thumb.jpg",
    hero: "/art/grave-thumb.jpg",
    servers: [],
  },
];

const previewAdmin: {
  unlocked: boolean;
  version: string;
  online: boolean;
  started: number;
  playerCount: number | null;
  phase: string;
  logs: AgentLogLine[];
} = {
  unlocked: false,
  version: "",
  online: false,
  started: 0,
  playerCount: null,
  phase: "idle",
  logs: [],
};

function previewLog(message: string) {
  previewAdmin.logs.push({
    id: previewAdmin.logs.length + 1,
    at: new Date().toISOString(),
    message,
  });
}

function previewStatus(): AgentStatus {
  return {
    version: previewAdmin.version,
    online: previewAdmin.online,
    uptimeSecs: previewAdmin.online ? Math.max(1, Math.floor((Date.now() - previewAdmin.started) / 1000)) : 0,
    playerCount: previewAdmin.online ? previewAdmin.playerCount : null,
    port: 7777,
    phase: previewAdmin.phase,
    agentVersion: "0.3.3",
    lastCheckAt: "2026-10-17T07:00:00Z",
    lastCheckResult: "up to date",
    lastUpdateAt: "2026-10-17T07:00:12Z",
    lastUpdateResult: "restarted",
    stagedVersion: "",
    nextRestartAt: "2026-10-18T03:00:00-04:00",
  };
}

const previewNotes = `## v0.9.5 – The Brawler's Update

- New arena: The Rusty Cage
- 4 new fighters join the roster
- Improved netcode & hit registration
- Performance optimizations
- Bug fixes and balance tweaks`;

function previewBootstrap(): Bootstrap {
  return {
    launcherVersion: "0.3.1",
    displayVersion: "v0.3.1",
    defaultInstallRoot: "%LOCALAPPDATA%\\uXa\\Games",
    dataDir: "%LOCALAPPDATA%\\uXa\\Launcher",
    logDir: "%LOCALAPPDATA%\\uXa\\Launcher\\logs",
    hasGithubToken: false,
    hasAgentSecret: previewAdmin.unlocked,
    settings: {
      installRoot: "",
      installOverrides: {},
      checkOnStart: true,
      newsUrl: "",
      selectedServer: { "friday-night-fights": "dolmar" },
      launcherUpdateOwner: "",
      launcherUpdateRepo: "",
    },
    games: previewGames,
    news: [
      {
        id: "ironfist-2v2",
        guild: "Ironfist Guild",
        ago: "2h ago",
        body: "We're hosting a 2v2 tournament this weekend!",
      },
      {
        id: "ravenwatch-hall",
        guild: "Ravenwatch Guild",
        ago: "1d ago",
        body: "New guild hall upgrades unlocked.",
      },
    ],
    launcherUpdateConfigured: false,
  };
}

const previewApi: LauncherApi = {
  async bootstrap() {
    return previewBootstrap();
  },
  async inspect(gameId) {
    if (gameId === "grave-march") {
      return {
        gameId,
        state: "ready",
        patchNotes: "Build 72, last updated Oct 13, 2026",
        message: "Browser preview. The desktop app reads this line from the live page.",
        webBuild: "72",
        webUpdated: "Oct 13, 2026",
        webUrl: "https://www.undeadlegion.pro/grave-march-unity/",
        releaseName: "Build 72",
      };
    }
    const installed = sessionStorage.getItem("uxa-preview-fnf");
    if (installed) {
      return {
        gameId,
        state: "ready",
        installedVersion: installed,
        remoteVersion: "v0.9.5",
        releaseName: "v0.9.5 – The Brawler's Update",
        patchNotes: previewNotes,
        message: "",
      };
    }
    return {
      gameId,
      state: "not-installed",
      remoteVersion: "v0.9.5",
      releaseName: "v0.9.5 – The Brawler's Update",
      patchNotes: previewNotes,
      message: "Browser preview cannot download the Windows client. Use the desktop app.",
    };
  },
  async startPatch(gameId) {
    sessionStorage.setItem("uxa-preview-fnf", "v0.9.5");
    return {
      gameId,
      state: "ready",
      installedVersion: "v0.9.5",
      remoteVersion: "v0.9.5",
      releaseName: "v0.9.5 – The Brawler's Update",
      patchNotes: previewNotes,
      message: "Preview only — nothing was downloaded.",
    };
  },
  async onPatch() {
    return () => undefined;
  },
  async play(gameId) {
    if (gameId === "grave-march") {
      window.open("https://www.undeadlegion.pro/grave-march-unity/", "_blank", "noopener");
      return { openedUrl: "https://www.undeadlegion.pro/grave-march-unity/" };
    }
    throw new Error("Launching FridayNightFights.exe requires the desktop app.");
  },
  async saveSettings() {},
  async setToken() {},
  async clearToken() {},
  async probe() {
    return "unknown";
  },
  async applyLauncherUpdate() {
    return { status: "current", latest: "", url: "", notes: "", message: "" };
  },
  async onLauncherUpdate() {
    return () => undefined;
  },
  async openLogs() {},
  async openUrl(url: string) {
    window.open(url, "_blank", "noopener");
  },
  async refreshNews() {
    return previewBootstrap().news;
  },
  async unlockAdmin(secret) {
    if (secret.trim().length < 8) {
      throw new Error("Admin key was rejected by the server agent.");
    }
    previewAdmin.unlocked = true;
    previewLog("admin key accepted");
    return previewStatus();
  },
  async adminProbe() {
    if (!previewAdmin.unlocked) throw new Error("Enter the admin key in Settings.");
    return previewStatus();
  },
  async forgetAdmin() {
    previewAdmin.unlocked = false;
    previewAdmin.online = false;
    previewLog("admin key cleared");
  },
  async adminReleases() {
    return [
      { tag: "v1.2.0", name: "Good server", prerelease: false, assetName: "FridayNightFights-Server-Win64.zip" },
      { tag: "v9.9.9-broken", name: "Broken server", prerelease: true, assetName: "FridayNightFights-Server-Win64.zip" },
    ];
  },
  async adminLogs(_serverId, after) {
    return previewAdmin.logs.filter((line) => line.id > after);
  },
  async adminDeploy(_serverId, tag) {
    previewAdmin.phase = "deploying";
    previewLog(`deploy started tag=${tag}`);
    previewLog("backed up Saved");
    previewLog("broadcast warning");
    previewAdmin.version = tag || "v1.2.0";
    previewAdmin.online = true;
    previewAdmin.started = Date.now();
    previewAdmin.playerCount = 2;
    previewAdmin.phase = "running";
    previewLog(`deploy finished version=${previewAdmin.version}`);
    return { ok: true, message: `Deployed ${previewAdmin.version}.`, status: previewStatus() };
  },
  async adminRestart() {
    previewLog("restart requested");
    previewLog("broadcast warning");
    previewAdmin.online = true;
    previewAdmin.started = Date.now();
    previewLog("restart finished");
    return { ok: true, message: "Server restarted.", status: previewStatus() };
  },
  async adminStop() {
    previewLog("stop requested");
    previewAdmin.online = false;
    previewAdmin.phase = "idle";
    previewLog("stop finished");
    return { ok: true, message: "Server stopped.", status: previewStatus() };
  },
};

export function launcherApi(): LauncherApi {
  return inTauri() ? tauriApi : previewApi;
}
