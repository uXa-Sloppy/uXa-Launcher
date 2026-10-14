//! Agent configuration. The admin key and GitHub token are never read from JSON.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub install_dir: PathBuf,
    pub executable: String,
    pub port: u16,
    pub args: Vec<String>,
    pub preserve: Vec<String>,
    pub owner: String,
    pub repo: String,
    pub asset_contains: Vec<String>,
    pub asset_pattern: String,
    pub include_prereleases: bool,
    pub api_base: String,
    pub github_token: Option<String>,
    pub listen: String,
    pub warn_seconds: u64,
    pub health_wait_secs: u64,
    pub quit_wait_secs: u64,
    pub require_tcp: bool,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    pub backup_dir: PathBuf,
    pub work_dir: PathBuf,
    pub log_dir: PathBuf,
    pub secret: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileConfig {
    install_dir: String,
    #[serde(default = "default_exe")]
    executable: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default = "default_preserve")]
    preserve: Vec<String>,
    #[serde(default = "default_owner")]
    owner: String,
    #[serde(default = "default_repo")]
    repo: String,
    #[serde(default = "default_contains")]
    asset_contains: Vec<String>,
    #[serde(default)]
    asset_pattern: String,
    #[serde(default = "default_true")]
    include_prereleases: bool,
    #[serde(default = "default_api")]
    api_base: String,
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_warn")]
    warn_seconds: u64,
    #[serde(default = "default_health")]
    health_wait_secs: u64,
    #[serde(default = "default_quit")]
    quit_wait_secs: u64,
    #[serde(default)]
    require_tcp: bool,
    #[serde(default)]
    tls_cert: String,
    #[serde(default)]
    tls_key: String,
    #[serde(default)]
    backup_dir: String,
    #[serde(default)]
    work_dir: String,
    #[serde(default)]
    log_dir: String,
}

fn default_exe() -> String {
    "FridayNightFightsServer.exe".into()
}
fn default_port() -> u16 {
    7777
}
fn default_preserve() -> Vec<String> {
    vec!["Saved".into(), "Config".into()]
}
fn default_owner() -> String {
    "uXa-Sloppy".into()
}
fn default_repo() -> String {
    "FridayNightFights".into()
}
fn default_contains() -> Vec<String> {
    vec!["Server".into(), "Win64".into()]
}
fn default_true() -> bool {
    true
}
fn default_api() -> String {
    "https://api.github.com".into()
}
fn default_listen() -> String {
    "0.0.0.0:8787".into()
}
fn default_warn() -> u64 {
    15
}
fn default_health() -> u64 {
    8
}
fn default_quit() -> u64 {
    20
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn reject_embedded_secrets(text: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|err| format!("agent config is not valid JSON: {err}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "agent config must be a JSON object".to_string())?;
    for key in ["secret", "token", "githubToken", "github_token", "agentSecret", "password"] {
        if obj.keys().any(|existing| existing.eq_ignore_ascii_case(key)) {
            return Err(format!(
                "Refusing to start: agent config contains \"{key}\". Put the admin key in UXA_AGENT_SECRET or agent.secret, and a GitHub token in UXA_GITHUB_TOKEN."
            ));
        }
    }
    Ok(())
}

fn config_path() -> Result<PathBuf, String> {
    if let Some(path) = env_nonempty("UXA_AGENT_CONFIG") {
        let path = PathBuf::from(path);
        if !path.is_file() {
            return Err(format!("UXA_AGENT_CONFIG does not exist: {}", path.display()));
        }
        return Ok(path);
    }
    let beside = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("agent.json");
    if beside.is_file() {
        return Ok(beside);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join("agent.json");
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    Err(
        "No agent.json. Set UXA_AGENT_CONFIG or place agent.json next to uxa-server-agent. See config/agent.example.json."
            .into(),
    )
}

fn load_secret(config_path: &Path) -> Result<String, String> {
    if let Some(value) = env_nonempty("UXA_AGENT_SECRET") {
        return Ok(value);
    }
    if let Some(path) = env_nonempty("UXA_AGENT_SECRET_FILE") {
        let path = PathBuf::from(path);
        if let Some(value) = read_trimmed(&path) {
            return Ok(value);
        }
        return Err(format!(
            "UXA_AGENT_SECRET_FILE is missing or empty: {}",
            path.display()
        ));
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("agent.secret"));
        }
    }
    if let Some(dir) = config_path.parent() {
        candidates.push(dir.join("agent.secret"));
    }
    for path in &candidates {
        if let Some(value) = read_trimmed(path) {
            return Ok(value);
        }
    }
    Err(
        "No admin key. Set UXA_AGENT_SECRET or UXA_AGENT_SECRET_FILE, or create agent.secret next to the agent. The agent does not ship with a key."
            .into(),
    )
}

pub fn load_config() -> Result<AgentConfig, String> {
    let path = config_path()?;
    let text = fs::read_to_string(&path).map_err(|err| format!("Could not read {}: {err}", path.display()))?;
    reject_embedded_secrets(&text)?;
    let file: FileConfig = serde_json::from_str(&text)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    let secret = load_secret(&path)?;
    let mut config = file.into_config(secret)?;
    if let Some(value) = env_nonempty("UXA_AGENT_INSTALL") {
        config.install_dir = PathBuf::from(value);
    }
    if let Some(value) = env_nonempty("UXA_AGENT_LISTEN") {
        config.listen = value;
    }
    if let Some(value) = env_nonempty("UXA_AGENT_TLS_CERT") {
        config.tls_cert = PathBuf::from(value);
    }
    if let Some(value) = env_nonempty("UXA_AGENT_TLS_KEY") {
        config.tls_key = PathBuf::from(value);
    }
    if let Some(value) = env_nonempty("UXA_GITHUB_API_BASE") {
        config.api_base = value;
    }
    if let Some(value) = env_nonempty("UXA_GITHUB_TOKEN") {
        config.github_token = Some(value);
    }
    config.finish_paths()?;
    Ok(config)
}

impl FileConfig {
    fn into_config(self, secret: String) -> Result<AgentConfig, String> {
        if self.install_dir.trim().is_empty() {
            return Err("installDir is required in agent.json".into());
        }
        if self.port == 0 {
            return Err("port must not be 0".into());
        }
        Ok(AgentConfig {
            install_dir: PathBuf::from(self.install_dir.trim()),
            executable: {
                let exe = self.executable.trim();
                if exe.is_empty() {
                    default_exe()
                } else {
                    exe.to_string()
                }
            },
            port: self.port,
            args: self.args,
            preserve: if self.preserve.is_empty() {
                default_preserve()
            } else {
                self.preserve
            },
            owner: nonempty(self.owner, default_owner()),
            repo: nonempty(self.repo, default_repo()),
            asset_contains: if self.asset_contains.is_empty() {
                default_contains()
            } else {
                self.asset_contains
            },
            asset_pattern: self.asset_pattern,
            include_prereleases: self.include_prereleases,
            api_base: nonempty(self.api_base, default_api()),
            github_token: None,
            listen: nonempty(self.listen, default_listen()),
            warn_seconds: self.warn_seconds,
            health_wait_secs: self.health_wait_secs,
            quit_wait_secs: self.quit_wait_secs.max(1),
            require_tcp: self.require_tcp,
            tls_cert: PathBuf::from(self.tls_cert.trim()),
            tls_key: PathBuf::from(self.tls_key.trim()),
            backup_dir: PathBuf::from(self.backup_dir.trim()),
            work_dir: PathBuf::from(self.work_dir.trim()),
            log_dir: PathBuf::from(self.log_dir.trim()),
            secret,
        })
    }
}

fn nonempty(value: String, fallback: String) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback
    } else {
        trimmed.to_string()
    }
}

impl AgentConfig {
    pub fn finish_paths(&mut self) -> Result<(), String> {
        if self.secret.trim().is_empty() {
            return Err("admin key is empty".into());
        }
        self.secret = self.secret.trim().to_string();
        if self.work_dir.as_os_str().is_empty() {
            let parent = self.install_dir.parent().filter(|p| !p.as_os_str().is_empty());
            self.work_dir = parent
                .unwrap_or(Path::new("."))
                .join(".uxa-agent");
        }
        if self.backup_dir.as_os_str().is_empty() {
            self.backup_dir = self.work_dir.join("backups");
        }
        if self.log_dir.as_os_str().is_empty() {
            self.log_dir = self.work_dir.join("logs");
        }
        let cert_set = !self.tls_cert.as_os_str().is_empty();
        let key_set = !self.tls_key.as_os_str().is_empty();
        if cert_set != key_set {
            return Err("Set both tlsCert and tlsKey, or leave both empty.".into());
        }
        Ok(())
    }

    pub fn tls_enabled(&self) -> bool {
        !self.tls_cert.as_os_str().is_empty() && !self.tls_key.as_os_str().is_empty()
    }
}
