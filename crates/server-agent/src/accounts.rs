//! FNF account server (pre-alpha invite-key sign-ups) as an agent-managed sidecar.
//!
//! Layout on the game PC:
//!   <accountServer.installDir>/python/python.exe      (embeddable Python, no install)
//!   <accountServer.installDir>/Tools/fnf_account_server.py (+ fnf_account_store.py ...)
//!   <game Saved>/Accounts/accounts.txt, invite_keys.txt      (data; preserved by deploys
//!                                                            and included in Saved backups)
//! The agent starts it, health-checks GET /v1/health on 127.0.0.1:<port>, restarts it
//! when it dies, swaps in a new package with rollback, and mints invite keys from a
//! SHA-256 only (the plaintext key never reaches this machine).

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Duration;

use launcher_core::{download_url, download_with_resume, extract_zip, find_release_by_tag, verify_download, DownloadRequest, ReleaseSource};
use serde::Serialize;

use crate::runtime::{Agent, Fail};

const PID_FILE: &str = "account-server-process.json";
const VERSION_FILE: &str = ".uxa-account-server-version";

const INVITE_PY: &str = r#"import os, sys
tools, path, digest, note = sys.argv[1:5]
sys.path.insert(0, tools)
import fnf_account_store as s
d = os.path.dirname(os.path.abspath(path))
os.makedirs(d, exist_ok=True)
with s.AccountStore(d)._lock():
    rows = s._read_table(path, s.KEY_FIELDS)
    if any(r["key_sha256"] == digest for r in rows):
        print("exists")
        sys.exit(3)
    rows.append({"key_sha256": digest, "status": "unused", "created_utc": s.utc_now(),
                 "note": s._dash(note), "group_id": "-", "used_utc": "-", "used_name": "-",
                 "pending_salt": "-", "pending_hash": "-"})
    s._write_table(path, s.KEY_HEADER, s.KEY_FIELDS, rows)
print("ok")
"#;

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AccountServerStatus {
    pub enabled: bool,
    pub installed: bool,
    pub version: String,
    pub running: bool,
    pub healthy: bool,
    pub port: u16,
    pub data_dir: String,
    pub accounts: u32,
    /// Sign-ups = invite keys spent (each makes Name, Name1, Name2).
    pub signups: u32,
    pub keys_unused: u32,
    pub keys_pending: u32,
    pub keys_used: u32,
    pub last_signup_utc: String,
}

fn table_rows(path: &Path) -> Vec<Vec<String>> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.split('\t').map(|c| c.to_string()).collect())
        .collect()
}

pub(crate) fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

impl Agent {
    pub(crate) fn accounts_data_dir(&self) -> PathBuf {
        let cfg = &self.config.account_server;
        if !cfg.data_dir.as_os_str().is_empty() {
            return cfg.data_dir.clone();
        }
        let install = &self.config.install_dir;
        if let Some(rel) = launcher_core::resolve_preserved(install, Path::new("Saved")).into_iter().next() {
            return install.join(rel).join("Accounts");
        }
        install.join("FridayNightFights").join("Saved").join("Accounts")
    }

    fn accounts_python(&self) -> PathBuf {
        let cfg = &self.config.account_server;
        let local = cfg.install_dir.join(&cfg.python);
        if local.is_file() {
            local
        } else {
            PathBuf::from(&cfg.python)
        }
    }

    fn accounts_tools(&self) -> PathBuf {
        let script = self.config.account_server.install_dir.join(&self.config.account_server.script);
        script.parent().map(Path::to_path_buf).unwrap_or_default()
    }

    pub(crate) fn accounts_installed(&self) -> bool {
        let cfg = &self.config.account_server;
        cfg.install_dir.join(&cfg.script).is_file()
    }

    pub(crate) async fn accounts_healthy(&self) -> bool {
        let url = format!("http://127.0.0.1:{}/v1/health", self.config.account_server.port);
        match tokio::time::timeout(Duration::from_secs(3), self.client.get(&url).send()).await {
            Ok(Ok(resp)) => resp.status().is_success(),
            _ => false,
        }
    }

    pub async fn accounts_status(&self) -> AccountServerStatus {
        let cfg = &self.config.account_server;
        let data = self.accounts_data_dir();
        let accounts = table_rows(&data.join("accounts.txt")).len() as u32;
        let keys = table_rows(&data.join("invite_keys.txt"));
        let mut status = AccountServerStatus {
            enabled: cfg.enabled,
            installed: self.accounts_installed(),
            version: fs::read_to_string(cfg.install_dir.join(VERSION_FILE)).unwrap_or_default().trim().to_string(),
            running: self.accounts_child_alive().await,
            healthy: false,
            port: cfg.port,
            data_dir: data.display().to_string(),
            accounts,
            ..Default::default()
        };
        for row in &keys {
            match row.get(1).map(String::as_str) {
                Some("unused") => status.keys_unused += 1,
                Some("pending") => status.keys_pending += 1,
                Some("used") => {
                    status.keys_used += 1;
                    if let Some(at) = row.get(5) {
                        if at != "-" && *at > status.last_signup_utc {
                            status.last_signup_utc = at.clone();
                        }
                    }
                }
                _ => {}
            }
        }
        status.signups = status.keys_used;
        status.healthy = self.accounts_healthy().await;
        if status.healthy {
            status.running = true;
        }
        status
    }

    async fn accounts_child_alive(&self) -> bool {
        let mut guard = self.accounts.lock().await;
        if let Some(child) = guard.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                return true;
            }
            *guard = None;
        }
        false
    }

    /// Start when installed and not already answering. Adopts a copy left by a previous agent.
    pub async fn accounts_start(&self) -> Result<(), Fail> {
        let cfg = &self.config.account_server;
        if !cfg.enabled {
            return Err(Fail::msg(400, "The account server is disabled in agent.json."));
        }
        if !self.accounts_installed() {
            return Err(Fail::msg(404, format!(
                "The account server is not installed ({} is missing). Deploy a release with an FNF_<version>_Accounts_Win64.zip package.",
                cfg.install_dir.join(&cfg.script).display()
            )));
        }
        if self.accounts_healthy().await {
            return Ok(());
        }
        if self.accounts_child_alive().await {
            // Started but not answering yet; give it a moment below.
        } else {
            let data = self.accounts_data_dir();
            fs::create_dir_all(&data).map_err(|e| Fail::msg(500, format!("accounts folder: {e}")))?;
            let _ = fs::create_dir_all(&self.config.log_dir);
            let log = self.config.log_dir.join("account-server.log");
            let out = OpenOptions::new().create(true).append(true).open(&log).map_err(|e| Fail::msg(500, e.to_string()))?;
            let err = out.try_clone().map_err(|e| Fail::msg(500, e.to_string()))?;
            let script = cfg.install_dir.join(&cfg.script);
            let mut command = Command::new(self.accounts_python());
            command
                .arg("-u")
                .arg(&script)
                .arg("--dir")
                .arg(&data)
                .arg("--host")
                .arg(&cfg.host)
                .arg("--port")
                .arg(cfg.port.to_string())
                .current_dir(&cfg.install_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::from(out))
                .stderr(Stdio::from(err));
            crate::runtime::detach_command(&mut command);
            let child = command.spawn().map_err(|e| {
                Fail::msg(500, format!("could not start the account server with {}: {e}", self.accounts_python().display()))
            })?;
            let _ = fs::write(self.config.work_dir.join(PID_FILE), format!("{{\"pid\":{}}}", child.id()));
            self.log(format!("account server started pid={} port={}", child.id(), cfg.port));
            *self.accounts.lock().await = Some(child);
        }
        for _ in 0..20 {
            if self.accounts_healthy().await {
                self.record_event("accounts-start", format!("account server healthy on TCP {}", cfg.port));
                return Ok(());
            }
            if !self.accounts_child_alive().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let tail = crate::runtime::tail_text(&self.config.log_dir.join("account-server.log"), 15);
        self.record_event("accounts-fail", "account server did not pass its health check");
        Err(Fail::msg(500, format!("The account server did not answer on TCP {}. Log tail:\n{tail}", cfg.port)))
    }

    pub async fn accounts_stop(&self) {
        if let Some(mut child) = self.accounts.lock().await.take() {
            let _ = child.kill();
            let _ = child.wait();
        } else if let Ok(text) = fs::read_to_string(self.config.work_dir.join(PID_FILE)) {
            if let Some(pid) = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("pid")?.as_u64()) {
                crate::runtime::kill_pid_pub(pid as u32);
            }
        }
        let _ = fs::remove_file(self.config.work_dir.join(PID_FILE));
        for _ in 0..10 {
            if !self.accounts_healthy().await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        self.log("account server stopped");
    }

    pub async fn accounts_restart(&self) -> Result<AccountServerStatus, Fail> {
        self.accounts_stop().await;
        self.accounts_start().await?;
        Ok(self.accounts_status().await)
    }

    /// Install the AccountServer package from release `tag` (empty = newest), with rollback.
    pub async fn accounts_deploy(&self, tag: &str) -> Result<AccountServerStatus, Fail> {
        let _guard = self.begin_real()?;
        self.set_phase("deploying-accounts");
        let result = self.accounts_deploy_inner(tag.trim()).await;
        self.set_phase("idle");
        match &result {
            Ok(v) => self.record_event("accounts-deploy", format!("account server {v} installed")),
            Err(e) => self.record_event("accounts-deploy-failed", e.message.clone()),
        }
        result?;
        Ok(self.accounts_status().await)
    }

    pub(crate) async fn accounts_deploy_inner(&self, tag: &str) -> Result<String, Fail> {
        let cfg = &self.config.account_server;
        let source = ReleaseSource {
            asset_contains: cfg.asset_contains.clone(),
            asset_pattern: String::new(),
            ..self.release_source()
        };
        let release = find_release_by_tag(&self.client, &source, tag)
            .await
            .map_err(|e| Fail::msg(502, format!("account server package: {e}")))?;
        let downloads = self.config.work_dir.join("downloads");
        fs::create_dir_all(&downloads).map_err(|e| Fail::msg(500, e.to_string()))?;
        let archive = downloads.join(Path::new(&release.asset.name).file_name().unwrap_or_default());
        let _ = fs::remove_file(&archive);
        let url = download_url(&release.asset).map_err(|e| Fail::msg(502, e.to_string()))?.to_string();
        download_with_resume(
            DownloadRequest { client: &self.client, url: &url, dest: &archive, expected_size: release.asset.size, token: self.config.github_token.as_deref() },
            |_| {},
        )
        .await
        .map_err(|e| Fail::msg(502, e.to_string()))?;
        verify_download(&archive, release.asset.size, release.asset.digest.as_deref()).map_err(|e| Fail::msg(502, e.to_string()))?;
        let install = cfg.install_dir.clone();
        let staging = install.with_file_name(format!("{}.staging", install.file_name().unwrap_or_default().to_string_lossy()));
        let previous = install.with_file_name(format!("{}.previous", install.file_name().unwrap_or_default().to_string_lossy()));
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|e| Fail::msg(500, e.to_string()))?;
        }
        extract_zip(&archive, &staging).map_err(|e| Fail::msg(500, e.to_string()))?;
        if !staging.join(&cfg.script).is_file() {
            let _ = fs::remove_dir_all(&staging);
            return Err(Fail::msg(500, format!("package {} has no {}", release.asset.name, cfg.script)));
        }
        fs::write(staging.join(VERSION_FILE), &release.tag).map_err(|e| Fail::msg(500, e.to_string()))?;
        // Data lives in the game's Saved/Accounts, never in the program folder. Copy it aside anyway.
        let data = self.accounts_data_dir();
        if data.is_dir() {
            let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
            let dest = self.config.backup_dir.join(format!("accounts-{stamp}"));
            let _ = crate::runtime::copy_dir(&data, &dest);
            self.log(format!("backed up accounts to {}", dest.display()));
        }
        self.accounts_stop().await;
        if previous.exists() {
            let _ = fs::remove_dir_all(&previous);
        }
        if install.exists() {
            fs::rename(&install, &previous).map_err(|e| Fail::msg(500, format!("could not move the old account server aside: {e}")))?;
        }
        if let Err(e) = fs::rename(&staging, &install) {
            if previous.exists() {
                let _ = fs::rename(&previous, &install);
            }
            let _ = self.accounts_start().await;
            return Err(Fail::msg(500, format!("swap failed: {e}. Previous account server restored.")));
        }
        if let Err(err) = self.accounts_start().await {
            self.log(format!("new account server failed: {}; rolling back", err.message));
            self.accounts_stop().await;
            if previous.exists() {
                let _ = fs::remove_dir_all(&install);
                let _ = fs::rename(&previous, &install);
                let back = self.accounts_start().await;
                return Err(Fail::msg(500, format!(
                    "Account server {} failed ({}). Previous version restored{}.",
                    release.tag,
                    err.message,
                    if back.is_ok() { " and running" } else { " but it did not start" }
                )));
            }
            return Err(err);
        }
        let _ = fs::remove_file(&archive);
        Ok(release.tag)
    }

    /// Append an unused key given only its SHA-256 (of the upper-cased key).
    pub async fn accounts_invite(&self, key_sha256: &str, note: &str) -> Result<(), Fail> {
        let digest = key_sha256.trim().to_ascii_lowercase();
        if !is_sha256_hex(&digest) {
            return Err(Fail::msg(400, "keySha256 must be 64 hex characters (SHA-256 of the upper-case key)."));
        }
        if !self.accounts_installed() {
            return Err(Fail::msg(404, "The account server is not installed."));
        }
        let note: String = note.chars().filter(|c| !c.is_control()).take(64).collect();
        let path = self.accounts_data_dir().join("invite_keys.txt");
        let output = tokio::task::spawn_blocking({
            let python = self.accounts_python();
            let tools = self.accounts_tools();
            move || {
                Command::new(python)
                    .arg("-c")
                    .arg(INVITE_PY)
                    .arg(tools)
                    .arg(path)
                    .arg(digest)
                    .arg(note)
                    .output()
            }
        })
        .await
        .map_err(|e| Fail::msg(500, e.to_string()))?
        .map_err(|e| Fail::msg(500, format!("could not run python: {e}")))?;
        match output.status.code() {
            Some(0) => {
                self.log("invite key added (hash only)");
                Ok(())
            }
            Some(3) => Err(Fail::msg(409, "That invite key is already in the list.")),
            _ => Err(Fail::msg(500, format!(
                "invite key was not added: {}",
                String::from_utf8_lossy(&output.stderr).chars().take(300).collect::<String>()
            ))),
        }
    }

    /// One watchdog tick: keep the account server up, restart a crashed game server.
    pub(crate) async fn watchdog_tick(&self) {
        if self.config.account_server.enabled && self.config.account_server.watchdog && self.accounts_installed()
            && !self.busy.load(Ordering::Acquire) && !self.accounts_healthy().await
        {
            self.log("watchdog: account server is not answering; starting it");
            if let Err(e) = self.accounts_start().await {
                self.log(format!("watchdog: account server start failed: {}", e.message));
            }
        }
        self.game_watchdog().await;
    }
}
