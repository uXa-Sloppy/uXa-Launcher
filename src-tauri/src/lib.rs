use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use launcher_core::{
    agent_exchange, cleanup_previous_launcher, command_label, download_launcher_update,
    execute_cli, failure_cli_result, http_client, inspect_game as inspect_release, load_catalog,
    load_news, load_servers, load_settings, patch_game, prepare_client_launch, probe_tcp,
    read_manifest, redact_cli, replace_executable, resolve_install_dir, resolve_update_repo,
    restore_previous_launcher, running_from_cargo_target, save_settings as write_settings,
    spawn_process, try_local_cli, verify_exe_file, write_cli_result, write_manifest, AgentAction,
    AgentLogLine, AgentReleaseSummary, AgentStatus, Catalog, CliInput, CliResultFile, GameDef,
    GameSource, GameStatus, InspectRequest, LauncherUpdateFetch, NewsItem, PatchJob, PatchProgress,
    PreparedLauncherUpdate, ReleaseSource, ServerEntry, ServerMap, Settings, EXIT_FAILURE,
    LAUNCHER_EXE_ASSET,
};
pub use launcher_core::{parse_cli, CliCommand, EXIT_USAGE};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tracing_subscriber::prelude::*;

const GAMES_JSON: &str = include_str!("../../config/games.json");
const SERVERS_JSON: &str = include_str!("../../config/servers.json");
const NEWS_JSON: &str = include_str!("../../config/news.json");

struct AppPaths {
    data_dir: PathBuf,
    log_dir: PathBuf,
    download_dir: PathBuf,
    manifest_dir: PathBuf,
    default_install_root: PathBuf,
}

struct AppState {
    paths: AppPaths,
    client: reqwest::Client,
    settings: Mutex<Settings>,
    patching: Mutex<HashSet<String>>,
    /// Admin key for this process. Never written to settings.json.
    agent_secret: Mutex<Option<String>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GameView {
    id: String,
    name: String,
    kind: String,
    thumbnail: String,
    hero: String,
    servers: Vec<ServerEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    launcher_version: String,
    display_version: String,
    default_install_root: String,
    data_dir: String,
    log_dir: String,
    has_github_token: bool,
    has_agent_secret: bool,
    settings: Settings,
    games: Vec<GameView>,
    news: Vec<NewsItem>,
    launcher_update_configured: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PatchEvent {
    game_id: String,
    phase: String,
    received: u64,
    total: u64,
    percent: u8,
    message: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LauncherUpdateEvent {
    phase: String,
    received: u64,
    total: u64,
    percent: u8,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LauncherUpdateOutcome {
    status: String,
    latest: String,
    url: String,
    notes: String,
    message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlayResult {
    pid: Option<u32>,
    opened_url: Option<String>,
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn app_paths() -> AppPaths {
    let data_dir = env_path("UXA_DATA_DIR").unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("uXa")
            .join("Launcher")
    });
    let default_install_root = env_path("UXA_INSTALL_ROOT").unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("uXa")
            .join("Games")
    });
    AppPaths {
        log_dir: data_dir.join("logs"),
        download_dir: data_dir.join("downloads"),
        manifest_dir: data_dir.join("manifests"),
        default_install_root,
        data_dir,
    }
}

fn api_base() -> String {
    std::env::var("UXA_GITHUB_API_BASE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "https://api.github.com".into())
}

fn init_logging(log_dir: &Path, console: bool) {
    let _ = fs::create_dir_all(log_dir);
    let appender = tracing_appender::rolling::daily(log_dir, "launcher.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer);
    let started = if console {
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .with(tracing_subscriber::fmt::layer())
            .try_init()
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(file_layer)
            .try_init()
    };
    let _ = started;
    // The guard must live for the process or the log file is empty.
    std::mem::forget(guard);
}

fn read_override(dir: &Path, name: &str, bundled: &str) -> Result<String, String> {
    let path = dir.join(name);
    if path.is_file() {
        return fs::read_to_string(&path)
            .map_err(|err| format!("Could not read {}: {err}", path.display()));
    }
    Ok(bundled.to_string())
}

fn load_library(paths: &AppPaths) -> Result<(Catalog, ServerMap), String> {
    let games = read_override(&paths.data_dir, "games.json", GAMES_JSON)?;
    let servers = read_override(&paths.data_dir, "servers.json", SERVERS_JSON)?;
    let catalog = load_catalog(&games).map_err(|err| format!("games.json: {err}"))?;
    let servers = load_servers(&servers).map_err(|err| format!("servers.json: {err}"))?;
    Ok((catalog, servers))
}

fn token_path(data_dir: &Path) -> PathBuf {
    data_dir.join("github-token")
}

fn read_token_file(data_dir: &Path) -> Option<String> {
    fs::read_to_string(token_path(data_dir))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(windows)]
fn keyring_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new("uXa Launcher", "github-token").map_err(|err| err.to_string())
}

#[cfg(windows)]
fn store_token_secure(token: &str) -> Result<(), String> {
    let entry = keyring_entry()?;
    if token.is_empty() {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    } else {
        entry.set_password(token).map_err(|err| err.to_string())
    }
}

#[cfg(windows)]
fn load_token_secure() -> Result<Option<String>, String> {
    let entry = keyring_entry()?;
    match entry.get_password() {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn load_token(data_dir: &Path) -> Option<String> {
    #[cfg(windows)]
    if let Ok(Some(token)) = load_token_secure() {
        return Some(token);
    }
    read_token_file(data_dir)
}

fn validate_token(token: &str) -> Result<&str, String> {
    let token = token.trim();
    if token.is_empty() || token.len() > 300 || token.chars().any(|ch| ch.is_whitespace()) {
        return Err("Enter a single GitHub token with no spaces.".into());
    }
    Ok(token)
}

fn write_token_file(data_dir: &Path, token: &str) -> Result<(), String> {
    let path = token_path(data_dir);
    fs::write(&path, token).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn save_token(data_dir: &Path, token: &str) -> Result<(), String> {
    let token = validate_token(token)?;
    #[cfg(windows)]
    {
        if store_token_secure(token).is_ok() {
            let _ = fs::remove_file(token_path(data_dir));
            tracing::info!("GitHub token stored in Windows Credential Manager");
            return Ok(());
        }
        tracing::warn!("Credential Manager unavailable; storing the token in a user-only file");
    }
    write_token_file(data_dir, token)?;
    tracing::info!(
        "GitHub token stored at {} (mode 0600 on Unix). Windows builds use Credential Manager when it is available.",
        token_path(data_dir).display()
    );
    Ok(())
}

fn clear_token(data_dir: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = store_token_secure("");
    }
    let path = token_path(data_dir);
    if path.exists() {
        fs::remove_file(path).map_err(|err| err.to_string())?;
    }
    tracing::info!("GitHub token cleared");
    Ok(())
}

fn manifest_file(paths: &AppPaths, game_id: &str) -> PathBuf {
    paths.manifest_dir.join(format!("{game_id}.json"))
}

fn find_game<'a>(catalog: &'a Catalog, game_id: &str) -> Result<&'a GameDef, String> {
    catalog
        .games
        .iter()
        .find(|game| game.id == game_id)
        .ok_or_else(|| format!("Unknown game {game_id}"))
}

fn views(catalog: &Catalog, servers: &ServerMap) -> Vec<GameView> {
    catalog
        .games
        .iter()
        .map(|game| {
            let list = game
                .servers_key
                .is_empty()
                .then(Vec::new)
                .unwrap_or_else(|| servers.get(&game.servers_key).cloned().unwrap_or_default());
            let list = apply_agent_override(list);
            GameView {
                id: game.id.clone(),
                name: game.name.clone(),
                kind: game.kind.clone(),
                thumbnail: game.thumbnail.clone(),
                hero: if game.hero.is_empty() {
                    game.thumbnail.clone()
                } else {
                    game.hero.clone()
                },
                servers: list,
            }
        })
        .collect()
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn apply_agent_override(mut servers: Vec<ServerEntry>) -> Vec<ServerEntry> {
    if let Some(url) = env_string("UXA_AGENT_URL") {
        for server in &mut servers {
            server.agent_url = url.clone();
        }
    }
    servers
}

fn update_repo(settings: &Settings, catalog: &Catalog) -> (String, String) {
    resolve_update_repo(
        &settings.launcher_update_owner,
        &settings.launcher_update_repo,
        &catalog.launcher.update_owner,
        &catalog.launcher.update_repo,
    )
}

async fn news_items(
    state: &AppState,
    settings: &Settings,
    catalog: &Catalog,
) -> Result<Vec<NewsItem>, String> {
    let configured = if !settings.news_url.trim().is_empty() {
        settings.news_url.trim().to_string()
    } else {
        catalog.news_url.trim().to_string()
    };
    if configured.is_empty() {
        return load_news(&read_override(
            &state.paths.data_dir,
            "news.json",
            NEWS_JSON,
        )?)
        .map_err(|err| format!("news.json: {err}"));
    }
    let response = state
        .client
        .get(&configured)
        .send()
        .await
        .map_err(|err| format!("News feed: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| format!("News feed: {err}"))?;
    if !status.is_success() {
        return Err(format!("News feed returned {status}"));
    }
    load_news(&text).map_err(|err| format!("News feed: {err}"))
}

fn emit_progress(app: &AppHandle, game_id: &str, progress: &PatchProgress) {
    let _ = app.emit(
        "patch-progress",
        PatchEvent {
            game_id: game_id.to_string(),
            phase: progress.phase.clone(),
            received: progress.received,
            total: progress.total,
            percent: progress.percent,
            message: progress.message.clone(),
        },
    );
}

fn source_for(game: &GameDef, token: Option<String>) -> Result<ReleaseSource, String> {
    match &game.source {
        GameSource::GithubRelease {
            owner,
            repo,
            asset_contains,
            asset_pattern,
            include_prereleases,
        } => Ok(ReleaseSource {
            api_base: api_base(),
            owner: owner.clone(),
            repo: repo.clone(),
            asset_contains: asset_contains.clone(),
            asset_pattern: asset_pattern.clone(),
            include_prereleases: *include_prereleases,
            token,
        }),
        GameSource::Web { .. } => Err(format!("{} is played in the browser.", game.name)),
    }
}

#[tauri::command]
async fn bootstrap(state: State<'_, AppState>) -> Result<Bootstrap, String> {
    let (catalog, servers) = load_library(&state.paths)?;
    let settings = state
        .settings
        .lock()
        .map_err(|err| err.to_string())?
        .clone();
    let news = match news_items(&state, &settings, &catalog).await {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!("using bundled guild news: {err}");
            load_news(NEWS_JSON).map_err(|nested| nested.to_string())?
        }
    };
    let (owner, repo) = update_repo(&settings, &catalog);
    Ok(Bootstrap {
        launcher_version: env!("CARGO_PKG_VERSION").to_string(),
        // Always the built version, so the sidebar cannot show a stale or shortened label (games.json said "v0.3").
        display_version: format!("v{}", env!("CARGO_PKG_VERSION")),
        default_install_root: state.paths.default_install_root.display().to_string(),
        data_dir: state.paths.data_dir.display().to_string(),
        log_dir: state.paths.log_dir.display().to_string(),
        has_github_token: load_token(&state.paths.data_dir).is_some(),
        has_agent_secret: agent_secret_available(&state),
        settings,
        games: views(&catalog, &servers),
        news,
        launcher_update_configured: !owner.trim().is_empty() && !repo.trim().is_empty(),
    })
}

#[tauri::command]
async fn inspect_game(
    state: State<'_, AppState>,
    game_id: String,
    remote: bool,
) -> Result<GameStatus, String> {
    let (catalog, _) = load_library(&state.paths)?;
    let game = find_game(&catalog, &game_id)?.clone();
    let manifest =
        read_manifest(&manifest_file(&state.paths, &game_id)).map_err(|err| err.to_string())?;
    let token = load_token(&state.paths.data_dir);
    let status = inspect_release(InspectRequest {
        client: &state.client,
        game: &game,
        manifest: manifest.as_ref(),
        api_base: &api_base(),
        token: token.as_deref(),
        remote,
    })
    .await
    .map_err(|err| err.to_string())?;
    tracing::info!(
        game = %game_id,
        state = %status.state,
        installed = ?status.installed_version,
        remote = ?status.remote_version,
        "inspected game"
    );
    if !status.message.is_empty() {
        tracing::info!(game = %game_id, "{}", status.message);
    }
    Ok(status)
}

#[tauri::command]
async fn start_patch(
    app: AppHandle,
    state: State<'_, AppState>,
    game_id: String,
) -> Result<GameStatus, String> {
    {
        let mut set = state.patching.lock().map_err(|err| err.to_string())?;
        if !set.insert(game_id.clone()) {
            return Err("This game is already patching.".into());
        }
    }
    let result = patch_inner(&app, &state, &game_id).await;
    if let Ok(mut set) = state.patching.lock() {
        set.remove(&game_id);
    }
    result
}

async fn patch_inner(
    app: &AppHandle,
    state: &AppState,
    game_id: &str,
) -> Result<GameStatus, String> {
    let (catalog, _) = load_library(&state.paths)?;
    let game = find_game(&catalog, game_id)?.clone();
    let settings = state
        .settings
        .lock()
        .map_err(|err| err.to_string())?
        .clone();
    let install_dir = resolve_install_dir(&settings, &state.paths.default_install_root, &game)
        .map_err(|err| err.to_string())?;
    let token = load_token(&state.paths.data_dir);
    let source = source_for(&game, token)?;
    let job = PatchJob {
        source,
        install_dir,
        download_dir: state.paths.download_dir.join(game_id),
        preserve: if game.preserve.is_empty() {
            vec!["Saved/SaveGames".into(), "Saved/Config".into()]
        } else {
            game.preserve.clone()
        },
        game_id: game_id.to_string(),
    };
    tracing::info!(game = %game_id, install = %job.install_dir.display(), "patch started");
    let app_emit = app.clone();
    let gid = game_id.to_string();
    let manifest = match patch_game(&state.client, &job, |progress| {
        emit_progress(&app_emit, &gid, &progress);
    })
    .await
    {
        Ok(manifest) => manifest,
        Err(err) => {
            let message = err.to_string();
            tracing::error!(game = %game_id, "{message}");
            emit_progress(
                app,
                game_id,
                &PatchProgress {
                    phase: "error".into(),
                    received: 0,
                    total: 0,
                    percent: 0,
                    message: message.clone(),
                },
            );
            return Err(message);
        }
    };
    write_manifest(&manifest_file(&state.paths, game_id), &manifest)
        .map_err(|err| err.to_string())?;
    tracing::info!(game = %game_id, version = %manifest.version, "patch finished");
    let token = load_token(&state.paths.data_dir);
    match inspect_release(InspectRequest {
        client: &state.client,
        game: &game,
        manifest: Some(&manifest),
        api_base: &api_base(),
        token: token.as_deref(),
        remote: true,
    })
    .await
    {
        Ok(status) => Ok(status),
        Err(err) => {
            tracing::warn!("patched but the follow-up check failed: {err}");
            Ok(GameStatus {
                game_id: game_id.to_string(),
                state: "ready".into(),
                installed_version: Some(manifest.version.clone()),
                remote_version: Some(manifest.version),
                release_name: None,
                patch_notes: String::new(),
                message: String::new(),
                web_build: None,
                web_updated: None,
                web_url: None,
            })
        }
    }
}

#[tauri::command]
async fn play(
    state: State<'_, AppState>,
    game_id: String,
    server_id: Option<String>,
) -> Result<PlayResult, String> {
    let (catalog, servers) = load_library(&state.paths)?;
    let game = find_game(&catalog, &game_id)?;
    if let GameSource::Web { url, .. } = &game.source {
        open_http(url)?;
        tracing::info!(game = %game_id, url, "opened browser");
        return Ok(PlayResult {
            pid: None,
            opened_url: Some(url.clone()),
        });
    }
    let settings = state
        .settings
        .lock()
        .map_err(|err| err.to_string())?
        .clone();
    let install_dir = resolve_install_dir(&settings, &state.paths.default_install_root, game)
        .map_err(|err| err.to_string())?;
    let list = servers.get(&game.servers_key).cloned().unwrap_or_default();
    let selected = pick_server(&settings, &game.id, &list, server_id.as_deref())?;
    let relative = if game.servers_config.trim().is_empty() {
        "Config/Servers.ini"
    } else {
        game.servers_config.trim()
    };
    let executable = if game.executable.trim().is_empty() {
        "FridayNightFights.exe"
    } else {
        game.executable.trim()
    };
    let plan = prepare_client_launch(launcher_core::ClientLaunch {
        install_dir: &install_dir,
        executable_name: executable,
        servers_config_relative: relative,
        servers: &list,
        selected,
    })
    .map_err(|err| err.to_string())?;
    let pid = spawn_process(&plan).map_err(|err| err.to_string())?;
    Ok(PlayResult {
        pid: Some(pid),
        opened_url: None,
    })
}

fn pick_server<'a>(
    settings: &Settings,
    game_id: &str,
    servers: &'a [ServerEntry],
    requested: Option<&str>,
) -> Result<&'a ServerEntry, String> {
    if let Some(id) = requested {
        if let Some(server) = servers.iter().find(|server| server.id == id) {
            return Ok(server);
        }
    }
    if let Some(id) = settings.selected_server.get(game_id) {
        if let Some(server) = servers.iter().find(|server| server.id == id.as_str()) {
            return Ok(server);
        }
    }
    servers
        .iter()
        .find(|server| server.default)
        .or_else(|| servers.first())
        .ok_or_else(|| "No servers are configured for this game.".into())
}

fn open_http(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|err| err.to_string())?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err("Only http(s) links can be opened.".into());
    }
    open::that(url).map_err(|err| format!("Could not open the browser: {err}"))
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    let path = state.paths.data_dir.join("settings.json");
    write_settings(&path, &settings).map_err(|err| err.to_string())?;
    *state.settings.lock().map_err(|err| err.to_string())? = settings;
    tracing::info!("settings saved");
    Ok(())
}

#[tauri::command]
fn set_github_token(state: State<'_, AppState>, token: String) -> Result<(), String> {
    save_token(&state.paths.data_dir, &token)
}

#[tauri::command]
fn clear_github_token(state: State<'_, AppState>) -> Result<(), String> {
    clear_token(&state.paths.data_dir)
}

#[tauri::command]
async fn probe_server(host: String, port: u16) -> Result<String, String> {
    Ok(probe_tcp(&host, port).await.to_string())
}

#[tauri::command]
fn open_logs(state: State<'_, AppState>) -> Result<(), String> {
    let _ = fs::create_dir_all(&state.paths.log_dir);
    open::that(&state.paths.log_dir).map_err(|err| format!("Could not open the log folder: {err}"))
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    open_http(&url)
}

#[tauri::command]
async fn apply_launcher_update(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<LauncherUpdateOutcome, String> {
    let (catalog, _) = load_library(&state.paths)?;
    let settings = state
        .settings
        .lock()
        .map_err(|err| err.to_string())?
        .clone();
    if !settings.check_on_start {
        return Ok(outcome("current", "", "", "", ""));
    }
    let (owner, repo) = update_repo(&settings, &catalog);
    let token = load_token(&state.paths.data_dir);
    let download_dir = state.paths.download_dir.join("launcher-update");
    let app_progress = app.clone();
    let fetched = download_launcher_update(
        &state.client,
        &api_base(),
        &owner,
        &repo,
        env!("CARGO_PKG_VERSION"),
        token.as_deref(),
        &download_dir,
        |progress| {
            let _ = app_progress.emit(
                "launcher-update",
                LauncherUpdateEvent {
                    phase: progress.phase,
                    received: progress.received,
                    total: progress.total,
                    percent: progress.percent,
                    message: progress.message,
                },
            );
        },
    )
    .await
    .map_err(|err| err.to_string())?;
    match fetched {
        LauncherUpdateFetch::UpToDate => Ok(outcome("current", "", "", "", "")),
        LauncherUpdateFetch::Skipped(message) => {
            tracing::info!("launcher update skipped: {message}");
            Ok(outcome("current", "", "", "", ""))
        }
        LauncherUpdateFetch::Ready(prepared) => install_or_announce(&app, prepared),
    }
}

fn outcome(
    status: &str,
    latest: &str,
    url: &str,
    notes: &str,
    message: &str,
) -> LauncherUpdateOutcome {
    LauncherUpdateOutcome {
        status: status.to_string(),
        latest: latest.to_string(),
        url: url.to_string(),
        notes: notes.to_string(),
        message: message.to_string(),
    }
}

fn install_or_announce(
    app: &AppHandle,
    prepared: PreparedLauncherUpdate,
) -> Result<LauncherUpdateOutcome, String> {
    let current = std::env::current_exe().map_err(|err| {
        format!(
            "Could not find the running launcher ({err}). The installed launcher was not changed."
        )
    })?;
    if !should_replace_running_exe(&current) {
        let message = if running_from_cargo_target(&current) {
            format!(
                "Launcher {} is available. This copy was started from a build folder, so it was not replaced.",
                prepared.latest
            )
        } else {
            format!(
                "Launcher {} is available. It installs into %LOCALAPPDATA%\\uXa Launcher\\{} on Windows.",
                prepared.latest, LAUNCHER_EXE_ASSET
            )
        };
        tracing::info!("{message}");
        return Ok(outcome(
            "available",
            &prepared.latest,
            &prepared.url,
            &prepared.notes,
            &message,
        ));
    }

    let _ = app.emit(
        "launcher-update",
        LauncherUpdateEvent {
            phase: "installing".into(),
            received: prepared.size,
            total: prepared.size,
            percent: 100,
            message: format!("Installing launcher {}", prepared.latest),
        },
    );
    replace_executable(&current, &prepared.exe_path).map_err(|err| err.to_string())?;
    if let Err(err) = verify_exe_file(&current, prepared.size, prepared.sha256.as_deref()) {
        return Err(restore_after(err.to_string(), &current));
    }
    if let Err(err) = relaunch_launcher(&current) {
        return Err(restore_after(err, &current));
    }
    let message = format!("Restarting into launcher {}.", prepared.latest);
    let _ = app.emit(
        "launcher-update",
        LauncherUpdateEvent {
            phase: "restarting".into(),
            received: prepared.size,
            total: prepared.size,
            percent: 100,
            message: message.clone(),
        },
    );
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(600));
        handle.exit(0);
    });
    Ok(outcome(
        "restarting",
        &prepared.latest,
        &prepared.url,
        &prepared.notes,
        &message,
    ))
}

fn restore_after(err: String, current: &std::path::Path) -> String {
    match restore_previous_launcher(current) {
        Ok(()) => format!("{err} The previous version was restored."),
        Err(restore_err) => {
            format!("{err} Restoring the previous launcher also failed ({restore_err}).")
        }
    }
}

fn should_replace_running_exe(exe: &std::path::Path) -> bool {
    if running_from_cargo_target(exe) {
        return false;
    }
    let Some(name) = exe.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !name.eq_ignore_ascii_case(LAUNCHER_EXE_ASSET) {
        return false;
    }
    cfg!(windows)
}

fn relaunch_launcher(exe: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x01000000;
        let spawn = |flags: u32| {
            let mut command = std::process::Command::new(exe);
            if let Some(dir) = exe.parent() {
                command.current_dir(dir);
            }
            command.creation_flags(flags).spawn()
        };
        if let Ok(child) =
            spawn(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
        {
            std::mem::forget(child);
            return Ok(());
        }
        match spawn(CREATE_NEW_PROCESS_GROUP) {
            Ok(child) => {
                std::mem::forget(child);
                Ok(())
            }
            Err(err) => Err(format!("Could not start the updated launcher ({err}).")),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        Err("Launcher self-update replaces the Windows executable. The installed launcher was not changed.".into())
    }
}

fn agent_secret_path(data_dir: &Path) -> PathBuf {
    data_dir.join("agent-secret")
}

fn read_agent_secret_file(data_dir: &Path) -> Option<String> {
    fs::read_to_string(agent_secret_path(data_dir))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(windows)]
fn agent_keyring() -> Result<keyring::Entry, String> {
    keyring::Entry::new("uXa Launcher", "agent-secret").map_err(|err| err.to_string())
}

#[cfg(windows)]
fn store_agent_secret_secure(secret: &str) -> Result<(), String> {
    let entry = agent_keyring()?;
    if secret.is_empty() {
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    } else {
        entry.set_password(secret).map_err(|err| err.to_string())
    }
}

#[cfg(windows)]
fn load_agent_secret_secure() -> Result<Option<String>, String> {
    let entry = agent_keyring()?;
    match entry.get_password() {
        Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn load_agent_secret(data_dir: &Path) -> Option<String> {
    #[cfg(windows)]
    if let Ok(Some(secret)) = load_agent_secret_secure() {
        return Some(secret);
    }
    read_agent_secret_file(data_dir)
}

fn validate_agent_secret(secret: &str) -> Result<&str, String> {
    let secret = secret.trim();
    if secret.is_empty() || secret.len() > 512 || secret.chars().any(|ch| ch.is_control()) {
        return Err("Enter the admin key with no line breaks.".into());
    }
    Ok(secret)
}

fn write_agent_secret_file(data_dir: &Path, secret: &str) -> Result<(), String> {
    let path = agent_secret_path(data_dir);
    fs::write(&path, secret).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn save_agent_secret(data_dir: &Path, secret: &str) -> Result<(), String> {
    let secret = validate_agent_secret(secret)?;
    #[cfg(windows)]
    {
        if store_agent_secret_secure(secret).is_ok() {
            let _ = fs::remove_file(agent_secret_path(data_dir));
            tracing::info!("admin key stored in Windows Credential Manager");
            return Ok(());
        }
        tracing::warn!("Credential Manager unavailable; storing the admin key in a user-only file");
    }
    write_agent_secret_file(data_dir, secret)?;
    tracing::info!("admin key stored for this user");
    Ok(())
}

fn clear_agent_secret(data_dir: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = store_agent_secret_secure("");
    }
    let path = agent_secret_path(data_dir);
    if path.exists() {
        fs::remove_file(path).map_err(|err| err.to_string())?;
    }
    tracing::info!("admin key cleared");
    Ok(())
}

fn agent_secret_available(state: &AppState) -> bool {
    state
        .agent_secret
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .is_some()
        || load_agent_secret(&state.paths.data_dir).is_some()
}

fn remember_agent_secret(state: &AppState, secret: &str) {
    if let Ok(mut slot) = state.agent_secret.lock() {
        *slot = Some(secret.to_string());
    }
}

fn current_agent_secret(state: &AppState) -> Result<String, String> {
    if let Some(secret) = state
        .agent_secret
        .lock()
        .map_err(|err| err.to_string())?
        .clone()
    {
        return Ok(secret);
    }
    if let Some(secret) = load_agent_secret(&state.paths.data_dir) {
        remember_agent_secret(state, &secret);
        return Ok(secret);
    }
    Err("Enter the admin key in Settings.".into())
}

fn find_server_by_id(servers: &ServerMap, id: &str) -> Result<ServerEntry, String> {
    for list in servers.values() {
        if let Some(server) = list.iter().find(|server| server.id == id) {
            return Ok(server.clone());
        }
    }
    Err("That server is not in the list.".into())
}

fn resolve_agent_url(server: &ServerEntry) -> Result<String, String> {
    if let Some(url) = env_string("UXA_AGENT_URL") {
        return Ok(url);
    }
    let url = server.agent_url.trim();
    if url.is_empty() {
        return Err(format!(
            "{} has no server agent address. Add agentUrl in servers.json.",
            server.name
        ));
    }
    Ok(url.to_string())
}

async fn agent_status_at(
    state: &AppState,
    server_id: &str,
    secret: &str,
) -> Result<AgentStatus, String> {
    let (_, servers) = load_library(&state.paths)?;
    let server = find_server_by_id(&servers, server_id)?;
    let url = resolve_agent_url(&server)?;
    let body = agent_exchange(&state.client, &url, secret, "GET", "/v1/status", b"")
        .await
        .map_err(|err| err.to_string())?;
    serde_json::from_str(&body).map_err(|err| err.to_string())
}

async fn agent_call(
    state: &AppState,
    server_id: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<String, String> {
    let secret = current_agent_secret(state)?;
    let (_, servers) = load_library(&state.paths)?;
    let server = find_server_by_id(&servers, server_id)?;
    let url = resolve_agent_url(&server)?;
    tracing::info!(server = %server.name, %method, %path, "admin request");
    agent_exchange(&state.client, &url, &secret, method, path, body)
        .await
        .map_err(|err| err.to_string())
}

#[tauri::command]
async fn unlock_admin(
    state: State<'_, AppState>,
    secret: String,
    server_id: String,
    remember: bool,
) -> Result<AgentStatus, String> {
    let secret = validate_agent_secret(&secret)?.to_string();
    let status = agent_status_at(&state, &server_id, &secret).await?;
    if remember {
        save_agent_secret(&state.paths.data_dir, &secret)?;
    } else {
        clear_agent_secret(&state.paths.data_dir)?;
    }
    remember_agent_secret(&state, &secret);
    tracing::info!(server = %server_id, remember, "admin key accepted");
    Ok(status)
}

#[tauri::command]
async fn admin_probe(state: State<'_, AppState>, server_id: String) -> Result<AgentStatus, String> {
    let secret = current_agent_secret(&state)?;
    match agent_status_at(&state, &server_id, &secret).await {
        Ok(status) => Ok(status),
        Err(err) if err.contains("rejected") => {
            if let Ok(mut slot) = state.agent_secret.lock() {
                *slot = None;
            }
            let _ = clear_agent_secret(&state.paths.data_dir);
            Err(err)
        }
        Err(err) => Err(err),
    }
}

#[tauri::command]
fn forget_admin(state: State<'_, AppState>) -> Result<(), String> {
    if let Ok(mut slot) = state.agent_secret.lock() {
        *slot = None;
    }
    clear_agent_secret(&state.paths.data_dir)
}

#[tauri::command]
async fn admin_releases(
    state: State<'_, AppState>,
    server_id: String,
) -> Result<Vec<AgentReleaseSummary>, String> {
    let body = agent_call(&state, &server_id, "GET", "/v1/releases", b"").await?;
    serde_json::from_str(&body).map_err(|err| err.to_string())
}

#[tauri::command]
async fn admin_logs(
    state: State<'_, AppState>,
    server_id: String,
    after: u64,
) -> Result<Vec<AgentLogLine>, String> {
    let body = agent_call(
        &state,
        &server_id,
        "GET",
        &format!("/v1/logs?after={after}"),
        b"",
    )
    .await?;
    let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|err| err.to_string())?;
    let lines = parsed
        .get("lines")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    serde_json::from_value(lines).map_err(|err| err.to_string())
}

#[tauri::command]
async fn admin_deploy(
    state: State<'_, AppState>,
    server_id: String,
    tag: String,
) -> Result<AgentAction, String> {
    let payload =
        serde_json::to_vec(&serde_json::json!({ "tag": tag })).map_err(|err| err.to_string())?;
    tracing::info!(server = %server_id, %tag, "admin deploy");
    let body = agent_call(&state, &server_id, "POST", "/v1/deploy", &payload).await?;
    serde_json::from_str(&body).map_err(|err| err.to_string())
}

#[tauri::command]
async fn admin_restart(
    state: State<'_, AppState>,
    server_id: String,
) -> Result<AgentAction, String> {
    tracing::info!(server = %server_id, "admin restart");
    let body = agent_call(&state, &server_id, "POST", "/v1/restart", b"").await?;
    serde_json::from_str(&body).map_err(|err| err.to_string())
}

#[tauri::command]
async fn admin_stop(state: State<'_, AppState>, server_id: String) -> Result<AgentAction, String> {
    tracing::info!(server = %server_id, "admin stop");
    let body = agent_call(&state, &server_id, "POST", "/v1/stop", b"").await?;
    serde_json::from_str(&body).map_err(|err| err.to_string())
}

#[tauri::command]
async fn refresh_news(state: State<'_, AppState>) -> Result<Vec<NewsItem>, String> {
    let (catalog, _) = load_library(&state.paths)?;
    let settings = state
        .settings
        .lock()
        .map_err(|err| err.to_string())?
        .clone();
    news_items(&state, &settings, &catalog).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let paths = app_paths();
    let _ = fs::create_dir_all(&paths.data_dir);
    let _ = fs::create_dir_all(&paths.log_dir);
    let _ = fs::create_dir_all(&paths.manifest_dir);
    let _ = fs::create_dir_all(&paths.download_dir);
    init_logging(&paths.log_dir, true);
    if let Ok(exe) = std::env::current_exe() {
        cleanup_previous_launcher(&exe);
    }
    let settings = load_settings(&paths.data_dir.join("settings.json")).unwrap_or_else(|err| {
        tracing::error!("settings.json could not be read ({err}); using defaults");
        Settings::default()
    });
    let client = http_client().expect("http client");
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data = %paths.data_dir.display(),
        games = %paths.default_install_root.display(),
        "uXa Launcher starting"
    );
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(AppState {
            paths,
            client,
            settings: Mutex::new(settings),
            patching: Mutex::new(HashSet::new()),
            agent_secret: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            bootstrap,
            inspect_game,
            start_patch,
            play,
            save_settings,
            set_github_token,
            clear_github_token,
            probe_server,
            open_logs,
            open_url,
            apply_launcher_update,
            refresh_news,
            unlock_admin,
            admin_probe,
            forget_admin,
            admin_releases,
            admin_logs,
            admin_deploy,
            admin_restart,
            admin_stop
        ])
        .run(tauri::generate_context!())
        .expect("error while running uXa Launcher");
}

pub struct HeadlessOutcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// `%LOCALAPPDATA%\uXa Launcher\cli-last.json`, or `UXA_CLI_RESULT` when set.
pub fn cli_result_path() -> PathBuf {
    if let Some(path) = env_path("UXA_CLI_RESULT") {
        return path;
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("uXa Launcher")
        .join("cli-last.json")
}

/// Run one CLI command. Does not open a window, does not install a launcher
/// update, and does not write the admin key or settings. The caller prints
/// `stdout` and `stderr` after the Windows console is attached.
pub fn run_headless(command: CliCommand) -> HeadlessOutcome {
    let paths = app_paths();
    let _ = fs::create_dir_all(&paths.data_dir);
    let _ = fs::create_dir_all(&paths.log_dir);
    let _ = fs::create_dir_all(&paths.manifest_dir);
    let _ = fs::create_dir_all(&paths.download_dir);
    init_logging(&paths.log_dir, false);
    let label = command_label(&command);
    if let Ok(exe) = std::env::current_exe() {
        cleanup_previous_launcher(&exe);
    }
    let version = env!("CARGO_PKG_VERSION");
    let games = match read_override(&paths.data_dir, "games.json", GAMES_JSON) {
        Ok(text) => text,
        Err(err) => return headless_failure(&label, version, redact_cli(&err, &[])),
    };
    let servers = match read_override(&paths.data_dir, "servers.json", SERVERS_JSON) {
        Ok(text) => text,
        Err(err) => return headless_failure(&label, version, redact_cli(&err, &[])),
    };
    let admin_key = load_agent_secret(&paths.data_dir);
    let github_token = load_token(&paths.data_dir);
    let api = api_base();
    let agent_env = env_string("UXA_AGENT_URL");
    let input = CliInput {
        launcher_version: version,
        data_dir: &paths.data_dir,
        install_root: &paths.default_install_root,
        games_json: &games,
        servers_json: &servers,
        api_base: &api,
        agent_url_override: agent_env.as_deref(),
        admin_key: admin_key.as_deref(),
        github_token: github_token.as_deref(),
    };
    let report = if let Some(report) = try_local_cli(&input, &command) {
        report
    } else {
        let client = match http_client() {
            Ok(client) => client,
            Err(err) => {
                let secrets = secret_refs(&admin_key, &github_token);
                return headless_failure(&label, version, redact_cli(&err.to_string(), &secrets));
            }
        };
        tauri::async_runtime::block_on(execute_cli(&client, &input, command))
    };
    let mut stderr = report.stderr.clone();
    let mut code = report.exit_code;
    let record = launcher_core::cli_result_record(&label, &report);
    if let Some(err) = persist_cli_result(&record) {
        stderr.push_str(&err);
        if code == 0 {
            code = EXIT_FAILURE;
        }
    }
    if code != 0 && !report.stderr.trim().is_empty() {
        tracing::error!("{}", report.stderr.trim());
    }
    HeadlessOutcome {
        code,
        stdout: report.stdout,
        stderr,
    }
}

fn headless_failure(command: &str, launcher_version: &str, message: String) -> HeadlessOutcome {
    let record = failure_cli_result(command, launcher_version, &message, EXIT_FAILURE);
    let mut stderr = if message.ends_with('\n') {
        message
    } else {
        format!("{message}\n")
    };
    if let Some(err) = persist_cli_result(&record) {
        stderr.push_str(&err);
    }
    tracing::error!("{}", stderr.trim());
    HeadlessOutcome {
        code: EXIT_FAILURE,
        stdout: String::new(),
        stderr,
    }
}

fn persist_cli_result(record: &CliResultFile) -> Option<String> {
    let path = cli_result_path();
    match write_cli_result(&path, record) {
        Ok(()) => {
            tracing::info!(
                command = %record.command,
                ok = record.ok,
                exit = record.exit_code,
                path = %path.display(),
                "cli result written"
            );
            None
        }
        Err(err) => {
            let line = format!("Could not write {}: {err}\n", path.display());
            tracing::error!("{line}");
            Some(line)
        }
    }
}

fn secret_refs<'a>(
    admin_key: &'a Option<String>,
    github_token: &'a Option<String>,
) -> Vec<&'a str> {
    [admin_key.as_deref(), github_token.as_deref()]
        .into_iter()
        .flatten()
        .collect()
}
