use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(default)]
    pub news_url: String,
    pub launcher: LauncherInfo,
    pub games: Vec<GameDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherInfo {
    pub version: String,
    #[serde(default = "default_display")]
    pub display_version: String,
    #[serde(default)]
    pub update_owner: String,
    #[serde(default)]
    pub update_repo: String,
}

fn default_display() -> String {
    "v0.3.2".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameDef {
    pub id: String,
    pub name: String,
    /// `unreal-client` downloads a Windows build. `web` opens a browser.
    pub kind: String,
    pub thumbnail: String,
    #[serde(default)]
    pub hero: String,
    #[serde(default)]
    pub install_dir_name: String,
    #[serde(default)]
    pub executable: String,
    #[serde(default)]
    pub servers_config: String,
    #[serde(default)]
    pub servers_key: String,
    #[serde(default)]
    pub preserve: Vec<String>,
    pub source: GameSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum GameSource {
    GithubRelease {
        owner: String,
        repo: String,
        #[serde(default)]
        asset_contains: Vec<String>,
        #[serde(default)]
        asset_pattern: String,
        #[serde(default = "default_true")]
        include_prereleases: bool,
    },
    Web {
        url: String,
        #[serde(default)]
        version_pattern: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerEntry {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub default: bool,
    /// Base URL of the uXa Server Agent. Empty when this host is not managed from the launcher.
    #[serde(default)]
    pub agent_url: String,
    /// Legacy (0.3.9): TCP port of a directly exposed account server on `host`. Only used when
    /// neither `accountUrl` nor a usable `agentUrl` is set. From 0.3.10 the agent proxies sign-ups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_port: Option<u16>,
    /// Public base URL for invite sign-ups, normally `<agent tunnel>/fnf-accounts`
    /// (the server agent proxies it to its local account server). Empty: derived from `agentUrl`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_url: String,
}

pub type ServerMap = HashMap<String, Vec<ServerEntry>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewsItem {
    pub id: String,
    pub guild: String,
    pub ago: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewsFile {
    items: Vec<NewsItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub install_root: String,
    #[serde(default)]
    pub install_overrides: HashMap<String, String>,
    #[serde(default = "default_true")]
    pub check_on_start: bool,
    #[serde(default)]
    pub news_url: String,
    #[serde(default)]
    pub selected_server: HashMap<String, String>,
    #[serde(default)]
    pub launcher_update_owner: String,
    #[serde(default)]
    pub launcher_update_repo: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            install_root: String::new(),
            install_overrides: HashMap::new(),
            check_on_start: true,
            news_url: String::new(),
            selected_server: HashMap::new(),
            launcher_update_owner: String::new(),
            launcher_update_repo: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledManifest {
    pub game_id: String,
    pub version: String,
    pub asset_name: String,
    pub install_path: String,
    pub installed_at: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GameStatus {
    pub game_id: String,
    /// `ready`, `not-installed`, `update-available`, or `check-failed`.
    pub state: String,
    pub installed_version: Option<String>,
    pub remote_version: Option<String>,
    pub release_name: Option<String>,
    pub patch_notes: String,
    pub message: String,
    pub web_build: Option<String>,
    pub web_updated: Option<String>,
    pub web_url: Option<String>,
}

pub fn load_catalog(text: &str) -> Result<Catalog> {
    Ok(serde_json::from_str(text)?)
}

pub fn load_servers(text: &str) -> Result<ServerMap> {
    Ok(serde_json::from_str(text)?)
}

/// Direct game address Dolmar's clients used before the playit.gg UDP tunnel.
pub(crate) const RETIRED_DOLMAR_GAME_HOST: &str = "71.71.45.128";

/// A user `servers.json` replaces the shipped catalog. When Dolmar's game host
/// in that file is still [`RETIRED_DOLMAR_GAME_HOST`], use the shipped host and
/// port. `agentUrl`, `accountPort`, and `accountUrl` stay as saved.
pub fn prefer_shipped_game_host(override_text: &str, bundled_text: &str) -> String {
    let (Ok(mut user), Ok(bundled)) = (load_servers(override_text), load_servers(bundled_text))
    else {
        return override_text.to_string();
    };
    let mut changed = false;
    for (key, entries) in user.iter_mut() {
        let Some(shipped_list) = bundled.get(key) else {
            continue;
        };
        for entry in entries.iter_mut() {
            if entry.id != "dolmar" || entry.host.trim() != RETIRED_DOLMAR_GAME_HOST {
                continue;
            }
            let Some(shipped) = shipped_list.iter().find(|server| server.id == entry.id) else {
                continue;
            };
            if entry.host == shipped.host && entry.port == shipped.port {
                continue;
            }
            entry.host = shipped.host.clone();
            entry.port = shipped.port;
            changed = true;
        }
    }
    if !changed {
        return override_text.to_string();
    }
    serde_json::to_string(&user).unwrap_or_else(|_| override_text.to_string())
}

pub fn load_news(text: &str) -> Result<Vec<NewsItem>> {
    let file: NewsFile = serde_json::from_str(text)?;
    Ok(file.items)
}

pub fn load_settings(path: &Path) -> Result<Settings> {
    if !path.exists() {
        return Ok(Settings::default());
    }
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

pub fn save_settings(path: &Path, settings: &Settings) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(settings)?;
    fs::write(path, text + "\n")?;
    Ok(())
}

pub fn read_manifest(path: &Path) -> Result<Option<InstalledManifest>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&text)?))
}

pub fn write_manifest(path: &Path, manifest: &InstalledManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(manifest)? + "\n")?;
    Ok(())
}

pub fn resolve_install_dir(
    settings: &Settings,
    default_root: &Path,
    game: &GameDef,
) -> Result<PathBuf> {
    if let Some(over) = settings.install_overrides.get(&game.id) {
        let trimmed = over.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    if game.install_dir_name.trim().is_empty() {
        return Err(Error::msg(format!(
            "{} has no install directory name",
            game.name
        )));
    }
    let root = if settings.install_root.trim().is_empty() {
        default_root.to_path_buf()
    } else {
        PathBuf::from(settings.install_root.trim())
    };
    Ok(root.join(&game.install_dir_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_catalog_parses() {
        let catalog = load_catalog(include_str!("../../../config/games.json")).unwrap();
        assert_eq!(catalog.games.len(), 2);
        assert_eq!(catalog.launcher.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(catalog.launcher.update_owner, "uXa-Sloppy");
        assert_eq!(catalog.launcher.update_repo, "uXa-Launcher");
        let servers = load_servers(include_str!("../../../config/servers.json")).unwrap();
        let list = servers.get("friday-night-fights").unwrap();
        assert_eq!(list[0].host, "scooter-seventeen.tun.ply.gg");
        assert_eq!(list[0].port, 38455);
        assert_eq!(list[0].agent_url, "https://71.71.45.128:8788");
        assert!(list[1].agent_url.is_empty());
        let news = load_news(include_str!("../../../config/news.json")).unwrap();
        assert_eq!(news.len(), 2);
    }

    #[test]
    fn install_override_wins_over_root() {
        let catalog = load_catalog(include_str!("../../../config/games.json")).unwrap();
        let game = &catalog.games[0];
        let mut settings = Settings::default();
        settings.install_root = r"D:\Games".into();
        let path = resolve_install_dir(&settings, Path::new("/default"), game).unwrap();
        assert_eq!(path, PathBuf::from(r"D:\Games").join("FridayNightFights"));
        settings
            .install_overrides
            .insert(game.id.clone(), r"E:\FNF".into());
        let path = resolve_install_dir(&settings, Path::new("/default"), game).unwrap();
        assert_eq!(path, PathBuf::from(r"E:\FNF"));
    }

    #[test]
    fn old_dolmar_override_takes_the_shipped_game_host() {
        let bundled = include_str!("../../../config/servers.json");
        let override_text = r#"{
          "friday-night-fights": [
            {
              "id": "dolmar",
              "name": "Dolmar's Server",
              "host": "71.71.45.128",
              "port": 7777,
              "default": true,
              "agentUrl": "https://seats.trycloudflare.com",
              "accountPort": 7901,
              "accountUrl": "http://accounts.example:7901"
            },
            {
              "id": "localhost",
              "name": "Localhost",
              "host": "71.71.45.128",
              "port": 7777
            }
          ]
        }"#;
        let merged = load_servers(&prefer_shipped_game_host(override_text, bundled)).unwrap();
        let list = &merged["friday-night-fights"];
        assert_eq!(list[0].host, "scooter-seventeen.tun.ply.gg");
        assert_eq!(list[0].port, 38455);
        assert_eq!(list[0].agent_url, "https://seats.trycloudflare.com");
        assert_eq!(list[0].account_port, Some(7901));
        assert_eq!(list[0].account_url, "http://accounts.example:7901");
        assert_eq!(list[1].host, "71.71.45.128");
        assert_eq!(list[1].port, 7777);

        let custom = r#"{"friday-night-fights":[{"id":"dolmar","name":"D","host":"other.example","port":1,"agentUrl":"https://keep.example"}]}"#;
        let kept = load_servers(&prefer_shipped_game_host(custom, bundled)).unwrap();
        assert_eq!(kept["friday-night-fights"][0].host, "other.example");
        assert_eq!(kept["friday-night-fights"][0].port, 1);
        assert_eq!(
            kept["friday-night-fights"][0].agent_url,
            "https://keep.example"
        );
        assert_eq!(prefer_shipped_game_host("not json", bundled), "not json");

        let shipped = load_servers(&prefer_shipped_game_host(bundled, bundled)).unwrap();
        assert_eq!(
            shipped["friday-night-fights"][0].host,
            "scooter-seventeen.tun.ply.gg"
        );
        assert_eq!(shipped["friday-night-fights"][0].port, 38455);
        assert_eq!(
            shipped["friday-night-fights"][0].agent_url,
            "https://71.71.45.128:8788"
        );
    }
}
