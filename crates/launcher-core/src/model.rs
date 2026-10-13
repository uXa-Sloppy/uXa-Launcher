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
    "v0.1".to_string()
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
#[serde(tag = "type", rename_all = "kebab-case", rename_all_fields = "camelCase")]
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

pub fn resolve_install_dir(settings: &Settings, default_root: &Path, game: &GameDef) -> Result<PathBuf> {
    if let Some(over) = settings.install_overrides.get(&game.id) {
        let trimmed = over.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    if game.install_dir_name.trim().is_empty() {
        return Err(Error::msg(format!("{} has no install directory name", game.name)));
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
        assert_eq!(catalog.launcher.version, "0.1.0");
        let servers = load_servers(include_str!("../../../config/servers.json")).unwrap();
        let list = servers.get("friday-night-fights").unwrap();
        assert_eq!(list[0].host, "71.71.45.128");
        assert_eq!(list[0].port, 7777);
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
}
