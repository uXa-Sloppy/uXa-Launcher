import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  Bootstrap,
  GameStatus,
  LauncherUpdate,
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
  checkLauncherUpdate(): Promise<LauncherUpdate | null>;
  openLogs(): Promise<void>;
  openUrl(url: string): Promise<void>;
  refreshNews(): Promise<NewsItem[]>;
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
  checkLauncherUpdate: () => invoke<LauncherUpdate | null>("check_launcher_update"),
  openLogs: () => invoke("open_logs"),
  openUrl: (url) => invoke("open_url", { url }),
  refreshNews: () => invoke<NewsItem[]>("refresh_news"),
};

const previewGames: Bootstrap["games"] = [
  {
    id: "friday-night-fights",
    name: "Friday Night Fights",
    kind: "unreal-client",
    thumbnail: "/art/fnf-thumb.jpg",
    hero: "/art/fnf-hero.jpg",
    servers: [
      { id: "dolmar", name: "Dolmar's Server", host: "71.71.45.128", port: 7777, default: true },
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

const previewNotes = `## v0.9.5 – The Brawler's Update

- New arena: The Rusty Cage
- 4 new fighters join the roster
- Improved netcode & hit registration
- Performance optimizations
- Bug fixes and balance tweaks`;

function previewBootstrap(): Bootstrap {
  return {
    launcherVersion: "0.1.0",
    displayVersion: "v0.1",
    defaultInstallRoot: "%LOCALAPPDATA%\\uXa\\Games",
    dataDir: "%LOCALAPPDATA%\\uXa\\Launcher",
    logDir: "%LOCALAPPDATA%\\uXa\\Launcher\\logs",
    hasGithubToken: false,
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
  async checkLauncherUpdate() {
    return null;
  },
  async openLogs() {},
  async openUrl(url: string) {
    window.open(url, "_blank", "noopener");
  },
  async refreshNews() {
    return previewBootstrap().news;
  },
};

export function launcherApi(): LauncherApi {
  return inTauri() ? tauriApi : previewApi;
}
