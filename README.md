# uXa Launcher

Desktop launcher for the uXa gaming guild, version **0.3.1**. It installs and patches Windows games, keeps player saves across updates, and opens browser games. Admins can also deploy the Friday Night Fights dedicated server. Launcher versions are not game versions.

![Friday Night Fights ready to play](docs/screenshots/friday-night-fights.png)

![Grave March, build read from the live page](docs/screenshots/grave-march.png)

![Settings](docs/screenshots/settings.png)

![Server admin after a mock deploy](docs/screenshots/admin.png)

## What it does

- **Game library** from `config/games.json`, so another game is a config change.
- **Friday Night Fights** (Unreal Engine client): finds the newest GitHub release of `uXa-Sloppy/FridayNightFights`, including pre-releases, and downloads the zip whose name contains `Client` and `Win64`. That filter is `assetContains` / `assetPattern` in the config.
- **Patching** compares the local manifest with the release tag. The download shows progress, can resume, and is checked (size, zip structure, and the GitHub `sha256` digest when the API sends one). The zip is extracted beside the install and swapped into place. If the swap fails, the previous install is put back.
- **Saves and config** listed in `preserve` (by default `Saved/SaveGames` and `Saved/Config`) are copied onto the new build before the swap.
- **Play** is disabled while the launcher is checking or patching. It is **Install** or **Update** when a build is needed, and green **Play** when the install matches the release. If GitHub cannot be reached but a copy is already installed, Play stays available.
- **Server.** `config/servers.json` lists servers. Dolmar's Server (`71.71.45.128:7777`) is the default, plus Localhost. Play launches `FridayNightFights.exe` with `host:port` as the first argument and writes `Config/Servers.ini` as `Name | Host | Port` lines. A small dot is a best-effort TCP probe; the game itself uses UDP, so "unknown" does not mean the server is down.
- **Grave March** opens `https://www.undeadlegion.pro/grave-march-unity/` in the default browser. The version line is read from that page (`Build 72, last updated Oct 13, 2026`). If the page cannot be read, Play still works.
- **Patch notes** render the GitHub release body (or Grave March's build line). **Guild news** loads a `news.json` URL from Settings, and falls back to the bundled feed.
- **Settings** cover the games folder, a per-game install path, an optional GitHub token, update checks on startup, the news URL, and the launcher's own update repository. On Windows the token is stored in Credential Manager; otherwise it is a user-only file. It is never written into `settings.json`.
- **Launcher updates.** With **Check for updates on start** on, startup looks at the latest GitHub release of `uXa-Sloppy/uXa-Launcher` (Settings can override the owner and repo). A newer release downloads `uxa-launcher.exe`, checks its size and the `SHA256SUMS.txt` entry, then on Windows renames the running exe to `uxa-launcher.exe.old`, puts the new file in its place, and restarts. The next start deletes the `.old` file. Settings stay in `%LOCALAPPDATA%\uXa\Launcher` (including a `servers.json` override). If the download, checksum, or swap fails, the previous exe is left running. A saved GitHub token is sent only when that repository is private, and it is never written to the log.
- **Logs** go to `%LOCALAPPDATA%\uXa\Launcher\logs` on Windows (`~/.local/share/uXa/Launcher/logs` on Linux).
- **Admin** is hidden until an admin key is accepted by a server agent. From there an admin can deploy a dedicated-server build, restart it, or stop it. The key is never written into `settings.json`.

Out of scope: an intro video, accounts, and login. This repo does not modify Friday Night Fights.

## Command line

The same `uxa-launcher.exe` accepts commands so a shell can deploy and patch without opening the window. On Windows the installed copy is `%LOCALAPPDATA%\uXa Launcher\uxa-launcher.exe`. These commands still run when the window is already open. They print plain progress to the console and return exit code 0 on success. A failure returns 1. An unknown flag returns 2.

Release builds are Windows GUI programs, so a double-clicked launch has no console. A command run from PowerShell or `cmd` attaches to that console; if there is no parent console, one is allocated. Redirected stdout and stderr (`> file`, `|`, and `2>`) are left alone, so a script can capture them. `AttachConsole` on 0.3.3 replaced those redirected handles and the text was written nowhere: `--version` and `--status` exited 0 with empty stdout, empty stderr, and no log line.

Every command also writes `%LOCALAPPDATA%\uXa Launcher\cli-last.json`. `UXA_CLI_RESULT` overrides that path. The file is replaced atomically and is the stable result for automation:

```json
{
  "command": "--status",
  "ok": true,
  "message": "Launcher: 0.3.3\nClient: not installed\nServer: Dolmar's Server\nServer version: v0.9.7.2\nServer online: yes",
  "versions": { "launcher": "0.3.3", "client": null, "server": "v0.9.7.2" },
  "timestamp": "2026-10-17T03:00:00Z",
  "exitCode": 0
}
```

`ok` is true only when `exitCode` is 0. `versions.client` and `versions.server` are null when that command did not learn them. The same line is appended to the launcher log under `%LOCALAPPDATA%\uXa\Launcher\logs`. The admin key and GitHub token are never written into the file or the log.

```powershell
& "$env:LOCALAPPDATA\uXa Launcher\uxa-launcher.exe" --status
& "$env:LOCALAPPDATA\uXa Launcher\uxa-launcher.exe" --update-client
& "$env:LOCALAPPDATA\uXa Launcher\uxa-launcher.exe" --deploy-server v1.2.0
& "$env:LOCALAPPDATA\uXa Launcher\uxa-launcher.exe" --version
& "$env:LOCALAPPDATA\uXa Launcher\uxa-launcher.exe" --help
```

- `--status` prints the launcher version, the installed Friday Night Fights client version, and the selected server's version and whether it is online. Those three versions are also `versions` in `cli-last.json`.
- `--update-client` downloads and installs the latest Friday Night Fights client patch, the same way Play's patch step does, and does not start the game. It prints the installed version.
- `--deploy-server <version>` deploys that server release tag to the selected server (Dolmar's Server unless Settings names another) through the uXa Server Agent. It waits until the agent finishes, then prints the agent's result and the running version.
- With no flags, the launcher window opens.

The server list is `config/servers.json`, or `%LOCALAPPDATA%\uXa\Launcher\servers.json` when that file is present (the same override the window uses, including a Cloudflare tunnel `agentUrl`). Deploy and status use the admin key already saved by unlocking admin in the window. They never print, log, or rewrite that key, a GitHub token, or `settings.json`. If the key is not saved yet, the command stops and asks you to unlock admin once in the window.

Installing 0.3.1 over an older copy, or letting the launcher replace its own exe, updates `%LOCALAPPDATA%\uXa Launcher\uxa-launcher.exe` only. Settings, `servers.json`, the admin key, and the GitHub token stay in `%LOCALAPPDATA%\uXa\Launcher`.

## Server admin

The launcher does not copy files onto the game machine itself. A small **uXa Server Agent** runs on that machine (for Dolmar, `71.71.45.128`) and the launcher sends it signed requests.

The agent listens on its own port (default `8788`) and does four things when asked to deploy:

1. Download the zip whose name contains `Server` and `Win64` from the selected GitHub release of `uXa-Sloppy/FridayNightFights-Releases`.
2. Warn the running `FridayNightFightsServer` on stdin (`say`, then `quit`), then copy `Saved` to a timestamped backup.
3. Swap the new build into place, keep `Saved` and `Config`, and leave the previous build at `{install}.previous`.
4. Start it with the configured arguments and `-port=7777`. If that start fails, the agent stays on the new build. It does not put the previous build back. `POST /v1/rollback` does that, and only when it is signed.

From 0.3.12 the agent does not install on a clock. `updateMode` is `on-demand` even when an older `agent.json` still says `autoUpdate.enabled`, `serverEnabled`, and `dailyRestart`. On startup it deletes any build staged for the old 03:00 America/New_York window and records `cancelled pending 03:00 install of <tag>`. After that it only polls GitHub (`checkIntervalMinutes`, 5–15, default 10) and writes what it found. `POST /v1/check` does the same poll immediately. Neither path downloads or installs. A game install happens only on signed `POST /v1/deploy`. A game restart happens only on signed `POST /v1/restart`. The agent executable replaces itself only on signed `POST /v1/agent/update`.

`Saved` is copied to a timestamped folder before an install. The last 7 copies are kept (`keepBackups`, and `0` keeps every copy). A `Saved` folder inside the downloaded zip is discarded when the live install has one, including `<Project>/Saved`. If the new build dies during the health wait, the server log slice, stdout/stderr, and crash dumps are copied to `.uxa-agent/failed-starts/` before anything can restart, and `/v1/status` reports `failedStart`. The previous install stays on disk until a signed rollback. The live `Saved` directory is not deleted to make room for the zip, and it is not restored automatically.

Version tags with two, three, or four numeric parts are ordered numerically. `v0.9.7.2` and `v0.9.7.2-build18g` are both newer than `0.9.6` and `0.9.6.1`. The same numbers without a suffix are newer than that suffix. Scheduled checks pick the highest version, not whichever release GitHub listed first. Launcher release checks use the same comparison.

```json
"autoUpdate": {
  "updateMode": "on-demand",
  "checkIntervalMinutes": 10,
  "onFailedStart": "stay",
  "enabled": true,
  "intervalHours": 6,
  "checkAfterStartSecs": 30,
  "checkTimes": [],
  "serverEnabled": true,
  "agentEnabled": true,
  "agentOwner": "uXa-Sloppy",
  "agentRepo": "uXa-Launcher",
  "agentAsset": "uxa-server-agent.exe",
  "restartTime": "03:00",
  "restartTimeZone": "America/New_York",
  "restartWindowMinutes": 10,
  "dailyRestart": true,
  "keepBackups": 7,
  "graceSeconds": 180
}
```

`checkIntervalMinutes` is the report-only poll. `enabled`, `dailyRestart`, `serverEnabled`, and `restartTime` are still accepted so an old file loads, and they do not install or restart the game. `graceSeconds` is how long a signed deploy waits when `Saved/uxa-status.json` says players are online during a maintenance-style stop. An unknown player count uses `warnSeconds` instead. The agent release is the public `uXa-Sloppy/uXa-Launcher` latest stable build; `UXA_GITHUB_TOKEN` is sent only if the anonymous request fails. `FNF` release checks keep using that token when it is already configured, because that repo may be private. Nothing in `agent.json` may be named secret or token.

Each check and install is appended to `updates.log` in the log directory and to the admin log. Status adds `agentVersion`, `updateMode`, `lastCheckAt`, `lastCheckResult`, `lastUpdateAt`, `lastUpdateResult`, `previousVersion`, and `failedStart` when a start failed. A manual Deploy still installs immediately. `PATCH /v1/config` accepts only `updateMode` (`on-demand`), `onFailedStart` (`stay`), `checkIntervalMinutes` (5–15), `owner` (`uXa-Sloppy`), and `repo` (a single repository name, kept under owner `uXa-Sloppy`).

One Windows admin command, `uxa-server-agent.exe --setup-fnf`, sets the `uXaServerAgent` service to LocalSystem with restart-on-failure and writes `updateMode`, `onFailedStart`, and `repo` into `agent.json`. It only changes that service and files under `C:\FNF`.

Signed diagnostics: `GET /v1/diag/logs`, `GET /v1/diag/crashes`, `GET /v1/diag/crashes/download`, `GET /v1/diag/events`, and `GET /v1/diag/prereqs`. Log and dump reads are limited to the install's `Saved/Logs` and `Saved/Crashes` and the agent work directory. Event-log entries are Application events for the Friday Night Fights server executable. Returned text redacts addresses and `key=` / `token=` values.

Every request carries `X-Uxa-Timestamp` and `X-Uxa-Signature`, an HMAC-SHA256 of `{unix_seconds}\n{METHOD}\n{path}\n{sha256(body)}`. The timestamp must be within five minutes. Set `tlsCert` and `tlsKey` in `agent.json` to serve HTTPS with those same signatures. Without a certificate the listener is HTTP, which is what the local mock uses; put a real certificate on a machine that faces the network, or terminate TLS in front of it. The launcher trusts the system certificate store.

There is no built-in admin key. The agent reads, in order, `UXA_AGENT_SECRET`, `UXA_AGENT_SECRET_FILE`, then `agent.secret` next to the executable or the config. A GitHub token, if the releases are private, is `UXA_GITHUB_TOKEN` only. If `agent.json` contains a secret or token field, the agent refuses to start. `config/agent.example.json` is the shape of that file.

`config/servers.json` points Dolmar's Server at `https://71.71.45.128:8788`. That address is not a secret. `UXA_AGENT_URL` overrides every server's agent address for a local test.

In the launcher, Settings has an admin key field. Unlock asks the agent for status. A 401 stays locked and shows no Admin section. Remembering the key stores it in Windows Credential Manager, or in a user-only `agent-secret` file otherwise. After it unlocks, Admin shows the server, the matching releases, Deploy & Restart, Restart only, Stop, a confirm step, and a live log with version, online, uptime, and player count. A second line shows the agent build, the last check, the last update, the staged server version, and the next restart time. Player count is read from `Saved/uxa-status.json` when the server writes one; otherwise it stays unknown.

### Install the agent on Windows

On the game server, from an elevated PowerShell, after `cargo build -p uxa-server-agent --release` (or after downloading the `uxa-server-agent-windows` workflow artifact):

```powershell
$env:UXA_AGENT_SECRET = "paste-the-guild-key"
powershell -ExecutionPolicy Bypass -File scripts\install-server-agent.ps1
```

Edit `C:\uXa\ServerAgent\agent.json` so `installDir` is the dedicated server folder, then `sc start uXaServerAgent`. The service name is `uXaServerAgent`. `agent.secret` is readable by SYSTEM and Administrators only. Foreground debugging, without the service, is `uxa-server-agent.exe` in that folder with the same variables.

Generate a certificate the launcher will trust, set `tlsCert` and `tlsKey`, and restart the service before pointing the launcher at `https://`. The game port stays `7777`. The agent port stays separate.

### Try a deploy against the mock

The Rust test `deploy_preserves_saves_and_rolls_back` does this without the UI: a local GitHub stand-in serves a good server zip and a zip that exits immediately, and the agent must keep `Saved`, broadcast `say` then `quit`, stay on the failed build, and restore the good build only after `POST /v1/rollback`.

To click through it, serve the mock (it now includes a real `Server` + `Win64` zip), point an agent at it, and override the launcher:

```bash
python3 scripts/mock_release_server.py --port 8899
UXA_AGENT_SECRET=dev-admin-key \
UXA_AGENT_CONFIG=/tmp/uxa-agent.json \
UXA_GITHUB_API_BASE=http://127.0.0.1:8899 \
  cargo run -p uxa-server-agent
```

`/tmp/uxa-agent.json` needs `installDir`, `listen` (for example `127.0.0.1:8791`), `apiBase` set to the mock, `warnSeconds` of `0` if you do not want to wait, and no secret field. Then:

```bash
UXA_AGENT_URL=http://127.0.0.1:8791 \
UXA_GITHUB_API_BASE=http://127.0.0.1:8899 \
UXA_DATA_DIR=/tmp/uxa-demo \
UXA_INSTALL_ROOT=/tmp/uxa-demo/Games \
  npm run tauri dev
```

Unlock with `dev-admin-key`. Deploy & Restart. The agent log should show the download, the save backup, and a healthy process. Stop shuts it down. A build that exits during the health wait stays installed, with the previous build kept for a signed rollback.

## Layout on disk

| | Windows | Linux dev |
| --- | --- | --- |
| Launcher data | `%LOCALAPPDATA%\uXa\Launcher` | `~/.local/share/uXa/Launcher` |
| Games | `%LOCALAPPDATA%\uXa\Games\FridayNightFights` | `~/.local/share/uXa/Games/FridayNightFights` |

Override either location with `UXA_DATA_DIR` and `UXA_INSTALL_ROOT`. A `games.json`, `servers.json`, or `news.json` placed in the launcher data directory replaces the copy shipped with the app.

Private releases: add a GitHub token with access to read release contents. Public releases need no token. `UXA_GITHUB_API_BASE` points the client at another host (used by the mock server below).

## Develop

Requirements: Node.js 22+, Rust stable, and on Linux the WebKitGTK 4.1 development packages Tauri documents.

```bash
npm install
npm test          # patch, resume, save preservation, launch arguments
npm run tauri dev
```

The dev window talks to `http://127.0.0.1:1420`. Opening that address in a normal browser shows the same interface without the desktop commands.

### Try a patch without the private repo

`uXa-Sloppy/FridayNightFights` is not visible without a token. A local stand-in serves the releases API and a client zip:

```bash
python3 scripts/mock_release_server.py --port 8899
UXA_GITHUB_API_BASE=http://127.0.0.1:8899 \
UXA_DATA_DIR=/tmp/uxa-demo \
UXA_INSTALL_ROOT=/tmp/uxa-demo/Games \
npm run tauri dev
```

Install from the launcher, put a file in `Saved/SaveGames`, change the tag in `.uxa-dev/mock-version.txt`, and restart. The new build replaces the old one and the save is still there. Play writes `Config/Servers.ini` and starts `FridayNightFights.exe` with the selected `host:port`.

## Windows build

The installer is an NSIS setup (current user, no admin). It installs to `%LOCALAPPDATA%\uXa Launcher\uxa-launcher.exe` and bootstraps WebView2 if Windows does not already have it.

On a Windows machine, from this directory:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1
```

The setup executable is written to `src-tauri\target\release\bundle\nsis\`.

You need Node.js 22+, Rust stable, and the Visual Studio C++ build tools. WebView2 is already present on current Windows 11 images and on GitHub-hosted runners.

Pushing `main`, pushing a `v*` tag, or running the workflow by hand runs `.github/workflows/windows-build.yml`. It uses Rust **1.99.0** and uploads `uxa-launcher.exe`, the NSIS installer, and `SHA256SUMS.txt` as the `uxa-launcher-windows` artifact. A version tag or a manual run also attaches those files to a GitHub Release. `scripts/build-windows.sh` runs the same tests on Linux and prints these steps; it does not cross-compile the installer.

## Tests

`npm test` runs `cargo test -p launcher-core -p uxa-server-agent`. The launcher tests cover:

- choosing the `Client` + `Win64` zip and ignoring server/Linux assets
- semver comparison of release tags, including four-part versions and suffixes such as `v0.9.7.2-build18g`
- the server agent staging an update without stopping the process, installing it only from `apply_maintenance`, and restoring `Saved` when that build exits
- resume via HTTP Range
- rejecting zip paths that escape the install directory
- a failed archive leaving the existing install untouched
- an update that keeps `Saved/SaveGames` and `Saved/Config`, drops other old files, and swaps the folder
- `Config/Servers.ini` format and `FridayNightFights.exe host:port`
- Grave March's build line, including a dead page that still allows play
- a private-repo 404 that leaves an installed game playable
- `--status`, `--update-client`, and `--deploy-server` leave `settings.json`, `servers.json`, the admin key, and the GitHub token byte-for-byte unchanged
- a CLI result file records `command`, `ok`, `message`, `versions`, `timestamp`, and `exitCode`, redacts a GitHub token, and a redirected stdout handle is not replaced by the console attach
- a launcher self-update, and a second install over the top of that exe, leave those same files unchanged

The server-agent test deploys a mock `Server` + `Win64` zip, keeps `Saved` and `Config`, records the warning and `quit`, rejects a bad signature, rolls back a build that exits immediately, then restarts and stops the restored process. A config file that contains a secret is refused.

## Config

`config/games.json` is the catalog. Friday Night Fights:

```json
"assetContains": ["Client", "Win64"],
"assetPattern": "",
"includePrereleases": true,
"preserve": ["Saved/SaveGames", "Saved/Config"]
```

`assetPattern` is an optional regex applied in addition to `assetContains`. Assets must be `.zip` files. `config/servers.json` is the server list. An optional `agentUrl` is the uXa Server Agent for that host. The dedicated server build is chosen with `Server` and `Win64`, which is separate from the client filter above. `config/agent.example.json` is the agent config, without a key.

## 0.3.9: full remote deploy, big assets, account server, monitoring

**Assets over 2 GB.** GitHub refuses release files of 2 GiB or more. Split a big zip with
`scripts/split-release-asset.ps1 -File <zip>` and upload `<zip>.001`, `<zip>.002`, ... and
`<zip>.parts.json` instead of the zip. The launcher (client patch) and the agent (server
deploy) download every part with resume, retry a part that fails its SHA-256 (3 tries),
join the parts, check the whole-file SHA-256 from the manifest, then extract as before.
A release with a single zip works exactly as before, and a single zip wins when both
exist. Launcher/agent 0.3.8 and older cannot read split releases, so ship 0.3.9 first.

**Account server.** The agent runs the FNF account server (invite-key sign-ups) from
`FNFAccountServer` next to the game server, on 127.0.0.1:7798 (`accountServer` in agent.json;
from 0.3.10 always loopback, see below).
Data stays in the game's `Saved/Accounts`, so deploys keep it and every Saved backup
includes it. Package it with `scripts/package-account-server.ps1` (embeddable Python, no
install on the game PC) and attach `FNF_<version>_Accounts_Win64.zip` to the game release.
A watchdog restarts it when its health check fails.

**New agent routes** (all signed): `GET /v1/monitor`, `GET /v1/accounts/status`,
`POST /v1/accounts/deploy {"tag"}`, `POST /v1/accounts/restart`,
`POST /v1/accounts/invite {"keySha256","note"}` (hash only; the plaintext key never
leaves the admin PC), `POST /v1/agent/update` (self-update now), `POST /v1/backups/restore {"name":"YYYYMMDD-HHMMSS"}` (backs up the current Saved first). The watchdog also
restarts a game server that died on its own (at most 3 times an hour) and records
starts, exits, crashes, deploys, rollbacks and wipes in `events.jsonl`.

**Monitoring.** `GET /v1/monitor` returns status, uptime, version, players (from the game's
`Saved/uxa-status.json`, else "Join succeeded" log lines), CPU/RAM/disk, recent events and
crashes, the game log tail, account server state and sign-up counts, and the backup list.
The launcher shows it under Server admin > Server status (refreshes every 15 s).

**CLI.** `uxa-launcher --agent GET /v1/monitor`, `--agent POST <path> <json>`, and
`--mint-invite <discord name>` (prints the key; only its hash goes to the server).

**Client.** The launcher passes `-FNFAccountServer=<spec>` to the game (see 0.3.10).

## 0.3.10: sign-ups through the agent (no TCP 7798 port forward)

**Agent proxy.** The account server binds 127.0.0.1 only (an old `"host": "0.0.0.0"` is
overridden). The agent forwards two public, unsigned routes on its own listener (8788 or its
Cloudflare tunnel) to it: `POST /fnf-accounts/v1/signup` and `GET /fnf-accounts/v1/health`.
It also accepts `/fnf-accounts:<port>/v1/...`, the path a 0.10.0 game client sends (below).
Limits: 4 KB bodies, 6 sign-ups per minute per client IP (`CF-Connecting-IP` behind a tunnel),
60 sign-ups and 120 health checks per minute overall, 10 s upstream timeout. Bodies (invite
keys, passwords) are never logged; the agent log only records the HTTP result. Everything
else on the agent still needs the signed admin key.

**Launcher.** `-FNFAccountServer` comes from, in order: the server's `accountUrl`; the same
field in the published `servers.json` on `uXa-Sloppy/FridayNightFights-Releases` main (only
`accountUrl` is read from it, cached for offline starts), so a new tunnel address reaches
players without a launcher release; `agentUrl` + `/fnf-accounts` when the agent is plain HTTP
or HTTPS on the default port (a tunnel); otherwise the legacy `<host>:<accountPort|7798>`.
Game clients up to 0.10.0 build `http://<spec>/v1/signup` from `host:port`, so a URL with a
path is passed as `host:port/fnf-accounts:port` (tunnels answer plain HTTP on port 80).
