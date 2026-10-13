use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use launcher_core::{
    check_self_update, http_client, inspect_game as inspect_release, load_catalog, load_news,
    load_servers, load_settings, patch_game, prepare_client_launch, probe_tcp, read_manifest,
    resolve_install_dir, save_settings as write_settings, spawn_process, write_manifest, Catalog,
    GameDef, GameSource, GameStatus, InspectRequest, NewsItem, PatchJob, PatchProgress,
    ReleaseSource, ServerEntry, ServerMap, Settings,
};
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

fn init_logging(log_dir: &Path) {
    let _ = fs::create_dir_all(log_dir);
    let appender = tracing_appender::rolling::daily(log_dir, "launcher.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        )
        .with(tracing_subscriber::fmt::layer())
        .try_init();
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
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|err| err.to_string())?;
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

fn update_repo(settings: &Settings, catalog: &Catalog) -> (String, String) {
    let owner = if settings.launcher_update_owner.trim().is_empty() {
        catalog.launcher.update_owner.clone()
    } else {
        settings.launcher_update_owner.clone()
    };
    let repo = if settings.launcher_update_repo.trim().is_empty() {
        catalog.launcher.update_repo.clone()
    } else {
        settings.launcher_update_repo.clone()
    };
    (owner, repo)
}

async fn news_items(state: &AppState, settings: &Settings, catalog: &Catalog) -> Result<Vec<NewsItem>, String> {
    let configured = if !settings.news_url.trim().is_empty() {
        settings.news_url.trim().to_string()
    } else {
        catalog.news_url.trim().to_string()
    };
    if configured.is_empty() {
        return load_news(&read_override(&state.paths.data_dir, "news.json", NEWS_JSON)?)
            .map_err(|err| format!("news.json: {err}"));
    }
    let response = state
        .client
        .get(&configured)
        .send()
        .await
        .map_err(|err| format!("News feed: {err}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|err| format!("News feed: {err}"))?;
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
    let settings = state.settings.lock().map_err(|err| err.to_string())?.clone();
    let news = match news_items(&state, &settings, &catalog).await {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!("using bundled guild news: {err}");
            load_news(NEWS_JSON).map_err(|nested| nested.to_string())?
        }
    };
    let (owner, repo) = update_repo(&settings, &catalog);
    Ok(Bootstrap {
        launcher_version: catalog.launcher.version.clone(),
        display_version: catalog.launcher.display_version.clone(),
        default_install_root: state.paths.default_install_root.display().to_string(),
        data_dir: state.paths.data_dir.display().to_string(),
        log_dir: state.paths.log_dir.display().to_string(),
        has_github_token: load_token(&state.paths.data_dir).is_some(),
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
    let manifest = read_manifest(&manifest_file(&state.paths, &game_id)).map_err(|err| err.to_string())?;
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
async fn start_patch(app: AppHandle, state: State<'_, AppState>, game_id: String) -> Result<GameStatus, String> {
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

async fn patch_inner(app: &AppHandle, state: &AppState, game_id: &str) -> Result<GameStatus, String> {
    let (catalog, _) = load_library(&state.paths)?;
    let game = find_game(&catalog, game_id)?.clone();
    let settings = state.settings.lock().map_err(|err| err.to_string())?.clone();
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
    write_manifest(&manifest_file(&state.paths, game_id), &manifest).map_err(|err| err.to_string())?;
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
    let settings = state.settings.lock().map_err(|err| err.to_string())?.clone();
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
async fn check_launcher_update(state: State<'_, AppState>) -> Result<Option<launcher_core::CheckUpdate>, String> {
    let (catalog, _) = load_library(&state.paths)?;
    let settings = state.settings.lock().map_err(|err| err.to_string())?.clone();
    let (owner, repo) = update_repo(&settings, &catalog);
    let token = load_token(&state.paths.data_dir);
    let update = check_self_update(
        &state.client,
        &api_base(),
        &owner,
        &repo,
        &catalog.launcher.version,
        token.as_deref(),
    )
    .await
    .map_err(|err| err.to_string())?;
    if let Some(update) = &update {
        tracing::info!(latest = %update.latest, "launcher update available");
    }
    Ok(update)
}

#[tauri::command]
async fn refresh_news(state: State<'_, AppState>) -> Result<Vec<NewsItem>, String> {
    let (catalog, _) = load_library(&state.paths)?;
    let settings = state.settings.lock().map_err(|err| err.to_string())?.clone();
    news_items(&state, &settings, &catalog).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let paths = app_paths();
    let _ = fs::create_dir_all(&paths.data_dir);
    let _ = fs::create_dir_all(&paths.log_dir);
    let _ = fs::create_dir_all(&paths.manifest_dir);
    let _ = fs::create_dir_all(&paths.download_dir);
    init_logging(&paths.log_dir);
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
            check_launcher_update,
            refresh_news
        ])
        .run(tauri::generate_context!())
        .expect("error while running uXa Launcher");
}
