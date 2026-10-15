# uXa Launcher

Desktop launcher for the uXa gaming guild, version **0.2.0**. It installs and patches Windows games, keeps player saves across updates, and opens browser games. Admins can also deploy the Friday Night Fights dedicated server. Launcher versions are not game versions.

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
- **Launcher updates** are a v0.1 stub: if an owner/repo is set, a newer GitHub release is announced. The launcher does not install that update itself.
- **Logs** go to `%LOCALAPPDATA%\uXa\Launcher\logs` on Windows (`~/.local/share/uXa/Launcher/logs` on Linux).
- **Admin** is hidden until an admin key is accepted by a server agent. From there an admin can deploy a dedicated-server build, restart it, or stop it. The key is never written into `settings.json`.

Out of scope: an intro video, accounts, and login. This repo does not modify Friday Night Fights.

## Server admin

The launcher does not copy files onto the game machine itself. A small **uXa Server Agent** runs on that machine (for Dolmar, `71.71.45.128`) and the launcher sends it signed requests.

The agent listens on its own port (default `8788`) and does four things when asked to deploy:

1. Download the zip whose name contains `Server` and `Win64` from the selected GitHub release of `uXa-Sloppy/FridayNightFights`.
2. Warn the running `FridayNightFightsServer` on stdin (`say`, then `quit`), then copy `Saved` to a timestamped backup.
3. Swap the new build into place and keep `Saved` and `Config`.
4. Start it with the configured arguments and `-port=7777`, wait until the process is still running, and put the previous build back if it exits.

Every request carries `X-Uxa-Timestamp` and `X-Uxa-Signature`, an HMAC-SHA256 of `{unix_seconds}\n{METHOD}\n{path}\n{sha256(body)}`. The timestamp must be within five minutes. Set `tlsCert` and `tlsKey` in `agent.json` to serve HTTPS with those same signatures. Without a certificate the listener is HTTP, which is what the local mock uses; put a real certificate on a machine that faces the network, or terminate TLS in front of it. The launcher trusts the system certificate store.

There is no built-in admin key. The agent reads, in order, `UXA_AGENT_SECRET`, `UXA_AGENT_SECRET_FILE`, then `agent.secret` next to the executable or the config. A GitHub token, if the releases are private, is `UXA_GITHUB_TOKEN` only. If `agent.json` contains a secret or token field, the agent refuses to start. `config/agent.example.json` is the shape of that file.

`config/servers.json` points Dolmar's Server at `https://71.71.45.128:8788`. That address is not a secret. `UXA_AGENT_URL` overrides every server's agent address for a local test.

In the launcher, Settings has an admin key field. Unlock asks the agent for status. A 401 stays locked and shows no Admin section. Remembering the key stores it in Windows Credential Manager, or in a user-only `agent-secret` file otherwise. After it unlocks, Admin shows the server, the matching releases, Deploy & Restart, Restart only, Stop, a confirm step, and a live log with version, online, uptime, and player count. Player count is read from `Saved/uxa-status.json` when the server writes one; otherwise it stays unknown.

### Install the agent on Windows

On the game server, from an elevated PowerShell, after `cargo build -p uxa-server-agent --release` (or after downloading the `uxa-server-agent-windows` workflow artifact):

```powershell
$env:UXA_AGENT_SECRET = "paste-the-guild-key"
powershell -ExecutionPolicy Bypass -File scripts\install-server-agent.ps1
```

Edit `C:\uXa\ServerAgent\agent.json` so `installDir` is the dedicated server folder, then `sc start uXaServerAgent`. The service name is `uXaServerAgent`. `agent.secret` is readable by SYSTEM and Administrators only. Foreground debugging, without the service, is `uxa-server-agent.exe` in that folder with the same variables.

Generate a certificate the launcher will trust, set `tlsCert` and `tlsKey`, and restart the service before pointing the launcher at `https://`. The game port stays `7777`. The agent port stays separate.

### Try a deploy against the mock

The Rust test `deploy_preserves_saves_and_rolls_back` does this without the UI: a local GitHub stand-in serves a good server zip and a zip that exits immediately, and the agent must keep `Saved`, broadcast `say` then `quit`, and restore the good build.

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

Unlock with `dev-admin-key`. Deploy & Restart. The agent log should show the download, the save backup, and a healthy process. Stop shuts it down. A build that exits during the health wait is rolled back; that path is what the automated test forces.

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

The installer is an NSIS setup (current user, no admin). It bootstraps WebView2 if Windows does not already have it.

On a Windows machine, from this directory:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1
```

The setup executable is written to `target\release\bundle\nsis\` (the Cargo workspace target directory).

You need Node.js 22+, Rust stable, and the Visual Studio C++ build tools. WebView2 is already present on current Windows 11 images and on GitHub-hosted runners.

Pushing `main` also runs `.github/workflows/windows-build.yml`, which tests the patcher and uploads the NSIS installer as the `uxa-launcher-windows` artifact. `scripts/build-windows.sh` runs the same tests on Linux and prints these steps; it does not cross-compile the installer.

## Tests

`npm test` runs `cargo test -p launcher-core -p uxa-server-agent`. The launcher tests cover:

- choosing the `Client` + `Win64` zip and ignoring server/Linux assets
- semver comparison of release tags
- resume via HTTP Range
- rejecting zip paths that escape the install directory
- a failed archive leaving the existing install untouched
- an update that keeps `Saved/SaveGames` and `Saved/Config`, drops other old files, and swaps the folder
- `Config/Servers.ini` format and `FridayNightFights.exe host:port`
- Grave March's build line, including a dead page that still allows play
- a private-repo 404 that leaves an installed game playable

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
