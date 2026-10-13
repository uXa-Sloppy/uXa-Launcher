# uXa Launcher

Desktop launcher for the uXa gaming guild, version **0.1.0**. It installs and patches Windows games, keeps player saves across updates, and opens browser games. It is its own project: launcher versions are not game versions.

![Friday Night Fights ready to play](docs/screenshots/friday-night-fights.png)

![Grave March, build read from the live page](docs/screenshots/grave-march.png)

![Settings](docs/screenshots/settings.png)

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

Out of scope for 0.1: an intro video, accounts, and login. This repo does not modify Friday Night Fights.

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

The setup executable is written to `src-tauri\target\release\bundle\nsis\`.

You need Node.js 22+, Rust stable, and the Visual Studio C++ build tools. WebView2 is already present on current Windows 11 images and on GitHub-hosted runners.

Pushing `main` also runs `.github/workflows/windows-build.yml`, which tests the patcher and uploads the NSIS installer as the `uxa-launcher-windows` artifact. `scripts/build-windows.sh` runs the same tests on Linux and prints these steps; it does not cross-compile the installer.

## Tests

`cargo test -p launcher-core` covers:

- choosing the `Client` + `Win64` zip and ignoring server/Linux assets
- semver comparison of release tags
- resume via HTTP Range
- rejecting zip paths that escape the install directory
- a failed archive leaving the existing install untouched
- an update that keeps `Saved/SaveGames` and `Saved/Config`, drops other old files, and swaps the folder
- `Config/Servers.ini` format and `FridayNightFights.exe host:port`
- Grave March's build line, including a dead page that still allows play
- a private-repo 404 that leaves an installed game playable

## Config

`config/games.json` is the catalog. Friday Night Fights:

```json
"assetContains": ["Client", "Win64"],
"assetPattern": "",
"includePrereleases": true,
"preserve": ["Saved/SaveGames", "Saved/Config"]
```

`assetPattern` is an optional regex applied in addition to `assetContains`. Assets must be `.zip` files. `config/servers.json` is the server list.
