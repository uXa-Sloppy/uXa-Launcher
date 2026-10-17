export interface ServerEntry {
  id: string;
  name: string;
  host: string;
  port: number;
  default?: boolean;
  agentUrl?: string;
}

export interface GameView {
  id: string;
  name: string;
  kind: "unreal-client" | "web" | string;
  thumbnail: string;
  hero: string;
  servers: ServerEntry[];
}

export interface NewsItem {
  id: string;
  guild: string;
  ago: string;
  body: string;
}

export interface Settings {
  installRoot: string;
  installOverrides: Record<string, string>;
  checkOnStart: boolean;
  newsUrl: string;
  selectedServer: Record<string, string>;
  launcherUpdateOwner: string;
  launcherUpdateRepo: string;
}

export interface Bootstrap {
  launcherVersion: string;
  displayVersion: string;
  defaultInstallRoot: string;
  dataDir: string;
  logDir: string;
  hasGithubToken: boolean;
  hasAgentSecret: boolean;
  settings: Settings;
  games: GameView[];
  news: NewsItem[];
  launcherUpdateConfigured: boolean;
}

export type GameState = "ready" | "not-installed" | "update-available" | "check-failed" | "checking";

export interface GameStatus {
  gameId: string;
  state: GameState | string;
  installedVersion?: string | null;
  remoteVersion?: string | null;
  releaseName?: string | null;
  patchNotes: string;
  message: string;
  webBuild?: string | null;
  webUpdated?: string | null;
  webUrl?: string | null;
}

export interface PatchProgress {
  gameId: string;
  phase: string;
  received: number;
  total: number;
  percent: number;
  message: string;
}

export interface LauncherUpdate {
  latest: string;
  url: string;
  notes: string;
}

export interface LauncherUpdateProgress {
  phase: string;
  received: number;
  total: number;
  percent: number;
  message: string;
}

export interface LauncherUpdateOutcome {
  status: "current" | "restarting" | "available" | string;
  latest: string;
  url: string;
  notes: string;
  message: string;
}

export interface PlayResult {
  pid?: number | null;
  openedUrl?: string | null;
}

export interface AgentStatus {
  version: string;
  online: boolean;
  uptimeSecs: number;
  playerCount: number | null;
  port: number;
  phase: string;
  agentVersion?: string;
  lastCheckAt?: string;
  lastCheckResult?: string;
  lastUpdateAt?: string;
  lastUpdateResult?: string;
  stagedVersion?: string;
  nextRestartAt?: string;
}

export interface AgentRelease {
  tag: string;
  name: string;
  prerelease: boolean;
  assetName: string;
}

export interface AgentLogLine {
  id: number;
  at: string;
  message: string;
}

export interface AgentAction {
  ok: boolean;
  message: string;
  status: AgentStatus;
}
