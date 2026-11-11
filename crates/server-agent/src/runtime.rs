//! Download, swap, and start the dedicated server. A failed start stays on the
//! new build. The previous build remains on disk for a signed rollback.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use launcher_core::{
    copy_preserved, download_url, download_with_resume, extract_zip, find_release_by_tag,
    http_client, list_matching_releases, previous_install_dir, resolve_preserved, restore_previous,
    swap_keeping_previous, verify_download, AgentHealth, AgentLogLine, AgentReleaseSummary,
    AgentStatus, DownloadRequest, FailedStart, ReleaseInfo, ReleaseSource,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::config::{AgentConfig, LivePolicy};
use crate::update::{load_state, UpdateState};

const VERSION_FILE: &str = ".uxa-server-version";

struct Running {
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    bridge: Option<Child>,
    command_file: Option<PathBuf>,
    pid: u32,
    bridge_pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProcessRecord {
    pid: u32,
    #[serde(default)]
    bridge_pid: Option<u32>,
    #[serde(default)]
    command_file: String,
}

pub(crate) enum StopStyle {
    /// Broadcast with `warnSeconds`, then quit. Used by manual deploy and restart.
    Warn,
    /// When players are online, wait up to `graceSeconds` for the match to empty.
    Grace,
}

pub struct Agent {
    pub config: AgentConfig,
    pub(crate) client: reqwest::Client,
    process: tokio::sync::Mutex<Option<Running>>,
    logs: Mutex<std::collections::VecDeque<AgentLogLine>>,
    next_log: AtomicU64,
    pub(crate) busy: AtomicBool,
    phase: Mutex<String>,
    started_at: Mutex<Option<Instant>>,
    version: Mutex<String>,
    online: AtomicBool,
    pub(crate) updates: Mutex<UpdateState>,
    pub(crate) scheduler_started: AtomicBool,
    pub(crate) accounts: tokio::sync::Mutex<Option<Child>>,
    /// True after a start; cleared by an admin stop. A dead process while this is set is a crash.
    pub(crate) expect_running: AtomicBool,
    pub(crate) crash_restarts: Mutex<Vec<Instant>>,
    pub(crate) booted_at: Instant,
    last_exit: Mutex<Option<i32>>,
    server_log_mark: AtomicU64,
    failed_start: Mutex<Option<FailedStart>>,
    agent_health: Mutex<Option<AgentHealth>>,
    policy: Mutex<LivePolicy>,
    prereq_retried: Mutex<String>,
}

#[derive(Debug)]
pub struct Fail {
    pub http: u16,
    pub message: String,
}

impl Fail {
    pub(crate) fn msg(http: u16, message: impl Into<String>) -> Self {
        Self {
            http,
            message: message.into(),
        }
    }
}

impl Agent {
    pub fn new(mut config: AgentConfig) -> Result<Arc<Self>, String> {
        config.finish_paths()?;
        let client = http_client().map_err(|err| err.to_string())?;
        let version = read_version(&config.install_dir);
        let adopted = load_running(&config);
        let online = adopted.is_some();
        let failed = if online {
            let _ = fs::remove_file(config.work_dir.join("failed-start.json"));
            None
        } else {
            load_persisted_failure(&config.work_dir)
        };
        let phase = if failed.is_some() {
            "failed-start"
        } else {
            "idle"
        };
        let policy = LivePolicy::from_config(&config);
        let agent = Arc::new(Self {
            updates: Mutex::new(load_state(&config.work_dir)),
            scheduler_started: AtomicBool::new(false),
            config,
            client,
            process: tokio::sync::Mutex::new(adopted),
            logs: Mutex::new(std::collections::VecDeque::new()),
            next_log: AtomicU64::new(1),
            busy: AtomicBool::new(false),
            phase: Mutex::new(phase.into()),
            started_at: Mutex::new(if online { Some(Instant::now()) } else { None }),
            version: Mutex::new(version),
            online: AtomicBool::new(online),
            accounts: tokio::sync::Mutex::new(None),
            expect_running: AtomicBool::new(online),
            crash_restarts: Mutex::new(Vec::new()),
            booted_at: Instant::now(),
            last_exit: Mutex::new(None),
            server_log_mark: AtomicU64::new(0),
            failed_start: Mutex::new(failed),
            agent_health: Mutex::new(None),
            policy: Mutex::new(policy),
            prereq_retried: Mutex::new(String::new()),
        });
        // A 0.3.11 staged release must not install at 03:00 after this process starts.
        agent.cancel_pending_scheduled_install();
        Ok(agent)
    }

    pub fn log(&self, message: impl Into<String>) {
        let message = crate::diag::redact(&message.into());
        tracing::info!("{message}");
        let id = self.next_log.fetch_add(1, Ordering::Relaxed);
        let line = AgentLogLine {
            id,
            at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            message,
        };
        if let Ok(mut logs) = self.logs.lock() {
            logs.push_back(line);
            while logs.len() > 400 {
                logs.pop_front();
            }
        }
    }

    pub fn logs_after(&self, after: u64) -> Vec<AgentLogLine> {
        let Ok(logs) = self.logs.lock() else {
            return Vec::new();
        };
        logs.iter()
            .filter(|line| line.id > after)
            .take(200)
            .cloned()
            .collect()
    }

    pub(crate) fn set_phase(&self, phase: &str) {
        if let Ok(mut slot) = self.phase.lock() {
            *slot = phase.to_string();
        }
    }

    pub(crate) fn phase_name(&self) -> String {
        self.phase
            .lock()
            .map(|phase| phase.clone())
            .unwrap_or_else(|_| "idle".into())
    }

    fn keep_failed_phase(&self) {
        if self.phase_name() != "failed-start" {
            self.set_phase("idle");
        }
    }

    pub(crate) fn policy_snapshot(&self) -> LivePolicy {
        self.policy
            .lock()
            .map(|policy| policy.clone())
            .unwrap_or_else(|err| err.into_inner().clone())
    }

    fn exit_code(&self) -> Option<i32> {
        self.last_exit.lock().ok().and_then(|slot| *slot)
    }

    pub fn publish_health(&self, listening: bool) {
        let state = self.updates.lock().ok();
        let health = AgentHealth {
            ok: listening,
            at: chrono::Utc::now().to_rfc3339(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            update_mode: self.policy_snapshot().update_mode,
            listening,
            pending_install_cancelled: state
                .as_ref()
                .is_some_and(|state| state.pending_install_cancelled),
            cancelled_staged_version: state
                .as_ref()
                .map(|state| state.cancelled_staged_version.clone())
                .unwrap_or_default(),
        };
        if let Ok(text) = serde_json::to_string(&health) {
            let _ = fs::create_dir_all(&self.config.work_dir);
            let _ = fs::write(self.config.work_dir.join("agent-health.json"), text);
        }
        if let Ok(mut slot) = self.agent_health.lock() {
            *slot = Some(health);
        }
        self.log(format!(
            "agent health ok={listening} version={}",
            env!("CARGO_PKG_VERSION")
        ));
    }

    pub fn patch_config(&self, patch: &serde_json::Value) -> Result<serde_json::Value, Fail> {
        let path = &self.config.source_path;
        if path.as_os_str().is_empty() || !path.is_file() {
            return Err(Fail::msg(500, "agent.json path is not available"));
        }
        let text = fs::read_to_string(path)
            .map_err(|err| Fail::msg(500, format!("could not read agent config: {err}")))?;
        let mut value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|err| Fail::msg(500, format!("agent config is not valid JSON: {err}")))?;
        let policy = crate::config::apply_config_patch(&mut value, patch)
            .map_err(|err| Fail::msg(400, err))?;
        let pretty =
            serde_json::to_string_pretty(&value).map_err(|err| Fail::msg(500, err.to_string()))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, format!("{pretty}\n"))
            .map_err(|err| Fail::msg(500, format!("could not write agent config: {err}")))?;
        fs::rename(&tmp, path)
            .map_err(|err| Fail::msg(500, format!("could not replace agent config: {err}")))?;
        if let Ok(mut slot) = self.policy.lock() {
            *slot = policy.clone();
        }
        self.log(format!(
            "config patched updateMode={} onFailedStart={} repo={}/{} checkIntervalMinutes={}",
            policy.update_mode,
            policy.on_failed_start,
            policy.owner,
            policy.repo,
            policy.check_interval_minutes
        ));
        self.record_event("config", "signed config patch applied");
        Ok(value)
    }

    pub async fn status(&self) -> AgentStatus {
        self.refresh_liveness().await;
        let online = self.online.load(Ordering::Acquire);
        let version = self.version.lock().map(|v| v.clone()).unwrap_or_default();
        let uptime_secs = if online {
            self.started_at
                .lock()
                .ok()
                .and_then(|slot| slot.map(|t| t.elapsed().as_secs()))
                .unwrap_or(0)
        } else {
            0
        };
        let mut phase = self
            .phase
            .lock()
            .map(|p| p.clone())
            .unwrap_or_else(|_| "idle".into());
        if phase == "idle" && online {
            phase = "running".into();
        }
        let update = self.updates.lock().ok();
        let policy = self.policy_snapshot();
        AgentStatus {
            version,
            online,
            uptime_secs,
            player_count: if online {
                player_count(&self.config.install_dir)
            } else {
                None
            },
            port: self.config.port,
            phase,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            last_check_at: update
                .as_ref()
                .map(|s| s.last_check_at.clone())
                .unwrap_or_default(),
            last_check_result: update
                .as_ref()
                .map(|s| s.last_check_result.clone())
                .unwrap_or_default(),
            last_update_at: update
                .as_ref()
                .map(|s| s.last_update_at.clone())
                .unwrap_or_default(),
            last_update_result: update
                .as_ref()
                .map(|s| s.last_update_result.clone())
                .unwrap_or_default(),
            staged_version: update
                .as_ref()
                .map(|s| s.staged_version.clone())
                .unwrap_or_default(),
            next_restart_at: String::new(),
            update_mode: policy.update_mode,
            previous_version: previous_version(&self.config.install_dir),
            failed_start: self.failed_start.lock().ok().and_then(|slot| slot.clone()),
            agent_health: self.agent_health.lock().ok().and_then(|slot| slot.clone()),
            check_interval_minutes: policy.check_interval_minutes,
        }
    }

    pub async fn releases(&self) -> Result<Vec<AgentReleaseSummary>, Fail> {
        self.log("listing matching server releases");
        let source = self.release_source();
        let found = list_matching_releases(&self.client, &source)
            .await
            .map_err(|err| Fail::msg(502, err.to_string()))?;
        if found.is_empty() {
            return Err(Fail::msg(404, "No release asset matched Server and Win64."));
        }
        Ok(found
            .into_iter()
            .map(|release| AgentReleaseSummary {
                tag: release.tag,
                name: release.name,
                prerelease: release.prerelease,
                asset_name: release.asset.name,
            })
            .collect())
    }

    pub async fn deploy(&self, tag: &str) -> Result<AgentStatus, Fail> {
        let _guard = self.begin_real()?;
        let label = if tag.trim().is_empty() {
            "latest".to_string()
        } else {
            tag.trim().to_string()
        };
        self.set_phase("deploying");
        self.log(format!("deploy started tag={label}"));
        let result = self.deploy_inner(tag.trim()).await;
        match &result {
            Ok(()) => self.set_phase("idle"),
            Err(_) => self.keep_failed_phase(),
        }
        match result {
            Ok(()) => {
                let status = self.status().await;
                self.log(format!(
                    "deploy finished version={} online={}",
                    status.version, status.online
                ));
                self.record_event(
                    "deploy",
                    format!("deployed {} online={}", status.version, status.online),
                );
                Ok(status)
            }
            Err(err) => {
                self.log(format!("deploy failed: {}", err.message));
                self.record_event("deploy-failed", err.message.clone());
                Err(err)
            }
        }
    }

    pub async fn restart(&self) -> Result<AgentStatus, Fail> {
        let _guard = self.begin_real()?;
        self.log("restart requested");
        self.set_phase("stopping");
        if let Err(err) = self.stop_running(true).await {
            self.set_phase("idle");
            self.log(format!("restart failed: {}", err.message));
            return Err(err);
        }
        self.set_phase("starting");
        let tag = read_version(&self.config.install_dir);
        if let Err(err) = self.boot_installed(&tag, false).await {
            self.keep_failed_phase();
            self.log(format!("restart failed: {}", err.message));
            return Err(err);
        }
        self.set_phase("idle");
        let status = self.status().await;
        self.log(format!("restart finished online={}", status.online));
        Ok(status)
    }

    pub async fn stop(&self) -> Result<AgentStatus, Fail> {
        let _guard = self.begin_real()?;
        self.expect_running.store(false, Ordering::Release);
        self.record_event("stop", "admin stopped the game server");
        self.log("stop requested");
        self.set_phase("stopping");
        let result = self.stop_running(true).await;
        self.set_phase("idle");
        match result {
            Ok(()) => {
                let status = self.status().await;
                self.log(format!("stop finished online={}", status.online));
                Ok(status)
            }
            Err(err) => {
                self.log(format!("stop failed: {}", err.message));
                Err(err)
            }
        }
    }

    /// Newest game-server log tails (Unreal Saved/Logs and the agent's stdout capture)
    /// plus the crash folders under Saved/Crashes, newest first. Read-only.
    pub fn server_log(&self) -> serde_json::Value {
        let install = &self.config.install_dir;
        let mut logs = Vec::new();
        let mut crashes = Vec::new();
        for rel in launcher_core::resolve_preserved(install, Path::new("Saved")) {
            let saved = install.join(&rel);
            if let Some(file) = newest_file(&saved.join("Logs"), Some("log")) {
                logs.push(log_entry(&file, 300));
            }
            if let Ok(dir) = fs::read_dir(saved.join("Crashes")) {
                let mut found: Vec<_> = dir.flatten().filter(|e| e.path().is_dir()).collect();
                found.sort_by_key(|e| std::cmp::Reverse(modified(&e.path())));
                for entry in found.into_iter().take(10) {
                    let path = entry.path();
                    let tail = newest_file(&path, Some("log"))
                        .map(|f| tail_lines(&f, 60))
                        .unwrap_or_default();
                    crashes.push(serde_json::json!({
                        "name": entry.file_name().to_string_lossy(),
                        "modified": stamp_of(&path),
                        "logTail": tail,
                    }));
                }
            }
        }
        if let Some(file) = newest_file(&self.config.log_dir, None) {
            logs.push(log_entry(&file, 200));
        }
        serde_json::json!({ "logs": logs, "crashes": crashes })
    }

    /// Wipe: graceful quit (then kill), back up every Saved tree, remove each Saved/SaveGames
    /// (Config and the rest of Saved stay), start the server again. Nothing is removed unless
    /// the backup succeeded.
    pub async fn wipe(&self) -> Result<AgentStatus, Fail> {
        let _guard = self.begin_real()?;
        self.log("wipe requested");
        let result = self.wipe_inner().await;
        self.set_phase("idle");
        match result {
            Ok(()) => {
                let status = self.status().await;
                self.log(format!("wipe finished online={}", status.online));
                self.record_event("wipe", "SaveGames wiped after a Saved backup");
                Ok(status)
            }
            Err(err) => {
                self.log(format!("wipe failed: {}", err.message));
                Err(err)
            }
        }
    }

    async fn wipe_inner(&self) -> Result<(), Fail> {
        if !self.config.install_dir.exists() {
            return Err(Fail::msg(404, "No server is installed; nothing to wipe."));
        }
        self.set_phase("stopping");
        self.stop_running(true).await?;
        self.log("wipe: server stopped");
        self.set_phase("backing-up");
        match backup_saved(&self.config.install_dir, &self.config.backup_dir) {
            Ok(Some(path)) => self.log(format!("wipe: backed up Saved to {}", path.display())),
            Ok(None) => self.log("wipe: no Saved folder to back up"),
            Err(err) => {
                self.log(format!(
                    "wipe: backup failed ({err}); nothing was deleted, restarting"
                ));
                let _ = self.start_installed().await;
                return Err(Fail::msg(
                    500,
                    format!(
                        "Save backup failed: {err}. Nothing was wiped; the server was restarted."
                    ),
                ));
            }
        }
        self.set_phase("wiping");
        let removed = wipe_save_games(&self.config.install_dir)
            .map_err(|err| Fail::msg(500, format!("wipe: could not clear SaveGames: {err}")));
        match removed {
            Ok(list) if list.is_empty() => self.log("wipe: no SaveGames folder found"),
            Ok(list) => {
                for path in list {
                    self.log(format!("wipe: cleared {}", path.display()));
                }
            }
            Err(err) => {
                self.log(&err.message);
                let _ = self.start_installed().await;
                return Err(err);
            }
        }
        self.set_phase("starting");
        self.start_installed().await?;
        self.log("wipe: server started");
        self.health_check().await.map_err(|err| {
            Fail::msg(
                500,
                format!("Wiped, but the restart failed its health check: {err}"),
            )
        })?;
        Ok(())
    }

    /// The atomic flag is the real lock. BusyGuard only exists so the HTTP layer
    /// can share the type; `begin_real` clears it when the action returns.
    pub(crate) fn begin_real(&self) -> Result<ActionGuard<'_>, Fail> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Fail::msg(409, "Another admin action is already running."));
        }
        Ok(ActionGuard { agent: self })
    }

    pub(crate) fn release_source(&self) -> ReleaseSource {
        let policy = self.policy_snapshot();
        ReleaseSource {
            api_base: self.config.api_base.clone(),
            owner: policy.owner,
            repo: policy.repo,
            asset_contains: self.config.asset_contains.clone(),
            asset_pattern: self.config.asset_pattern.clone(),
            include_prereleases: self.config.include_prereleases,
            token: self.config.github_token.clone(),
        }
    }

    async fn deploy_inner(&self, tag: &str) -> Result<(), Fail> {
        let source = self.release_source();
        let release = find_release_by_tag(&self.client, &source, tag)
            .await
            .map_err(|err| Fail::msg(502, err.to_string()))?;
        self.log(format!(
            "selected release {} asset {}",
            release.tag, release.asset.name
        ));
        let staging = staging_dir(&self.config.install_dir)?;
        self.download_release(&release, &staging)
            .await
            .map_err(|err| Fail::msg(502, err))?;
        self.install_prepared(&staging, &release.tag, StopStyle::Warn, false)
            .await?;
        self.discard_stage_if_not_newer();
        Ok(())
    }

    /// Download a matching zip, verify it, and extract it to `dest`.
    /// The live install and the running server are left untouched.
    pub(crate) async fn download_release(
        &self,
        release: &ReleaseInfo,
        dest: &Path,
    ) -> Result<(), String> {
        let url = download_url(&release.asset)
            .map_err(|err| err.to_string())?
            .to_string();
        let downloads = self.config.work_dir.join("downloads");
        fs::create_dir_all(&downloads).map_err(|err| err.to_string())?;
        let archive = downloads.join(file_name(&release.asset.name));
        self.log(format!(
            "downloading {} ({} bytes)",
            release.asset.name, release.asset.size
        ));
        if release.asset.split.is_some() {
            let parts_dir = downloads.join(format!("{}.parts", file_name(&release.asset.name)));
            let parts = release
                .asset
                .split
                .as_ref()
                .map(|s| s.parts.len())
                .unwrap_or(0);
            self.log(format!(
                "{} is split into {parts} parts; downloading each with resume",
                release.asset.name
            ));
            let manifest = launcher_core::download_split(
                launcher_core::SplitDownload {
                    client: &self.client,
                    asset: &release.asset,
                    dest: &archive,
                    parts_dir: &parts_dir,
                    token: self.config.github_token.as_deref(),
                },
                |_| {},
            )
            .await
            .map_err(|err| err.to_string())?;
            let _ = fs::remove_dir_all(&parts_dir);
            self.log(format!(
                "joined {} parts, sha256 {} verified",
                manifest.parts.len(),
                manifest.sha256
            ));
        } else {
            download_with_resume(
                DownloadRequest {
                    client: &self.client,
                    url: &url,
                    dest: &archive,
                    expected_size: release.asset.size,
                    token: self.config.github_token.as_deref(),
                },
                |_| {},
            )
            .await
            .map_err(|err| err.to_string())?;
            verify_download(
                &archive,
                release.asset.size,
                release.asset.digest.as_deref(),
            )
            .map_err(|err| err.to_string())?;
        }
        let partial_name = format!(
            "{}.partial",
            dest.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("stage")
        );
        let partial = dest.with_file_name(partial_name);
        if partial.exists() {
            fs::remove_dir_all(&partial).map_err(|err| err.to_string())?;
        }
        extract_zip(&archive, &partial).map_err(|err| err.to_string())?;
        write_version(&partial, &release.tag).map_err(|err| err.to_string())?;
        if dest.exists() {
            fs::remove_dir_all(dest).map_err(|err| err.to_string())?;
        }
        fs::rename(&partial, dest).map_err(|err| err.to_string())?;
        if let Err(err) = crate::prereq::record_hashes(&self.config.work_dir, &release.tag, dest) {
            self.log(format!("could not record prerequisite hashes: {err}"));
        }
        self.log(format!("extracted {}", release.asset.name));
        Ok(())
    }

    /// Stop, back up Saved, overlay preserved paths onto `staging`, swap, and health-check.
    /// `keep_stage_on_error` leaves `staging` in place when backup or overlay fails
    /// so a staged download can be retried. The live Saved tree is never deleted here;
    /// a packaged Saved is removed only inside `staging`.
    pub(crate) async fn install_prepared(
        &self,
        staging: &Path,
        tag: &str,
        stop: StopStyle,
        keep_stage_on_error: bool,
    ) -> Result<(), Fail> {
        self.set_phase("stopping");
        self.stop_for(&stop).await?;
        let saved_backup = if self.config.install_dir.exists() {
            match backup_saved(&self.config.install_dir, &self.config.backup_dir) {
                Ok(path) => {
                    if let Some(path) = &path {
                        self.log(format!("backed up Saved to {}", path.display()));
                        prune_backups(
                            &self.config.backup_dir,
                            self.config.auto_update.keep_backups,
                        );
                    } else {
                        self.log("no Saved folder to back up");
                    }
                    path
                }
                Err(err) => {
                    if !keep_stage_on_error {
                        let _ = fs::remove_dir_all(staging);
                    }
                    let _ = self.start_installed().await;
                    return Err(Fail::msg(
                        500,
                        format!("save backup failed: {err}. The current build was restarted."),
                    ));
                }
            }
        } else {
            None
        };
        if self.config.install_dir.exists() {
            if let Err(err) =
                overlay_preserved(&self.config.install_dir, staging, &self.config.preserve)
            {
                if !keep_stage_on_error {
                    let _ = fs::remove_dir_all(staging);
                }
                let _ = self.start_installed().await;
                return Err(Fail::msg(
                    500,
                    format!("could not preserve saves and config: {err}. The current build was restarted."),
                ));
            }
            self.log(format!("preserved {}", self.config.preserve.join(", ")));
        }
        write_version(staging, tag).map_err(|err| Fail::msg(500, err.to_string()))?;

        self.set_phase("deploying");
        if let Err(err) = swap_keeping_previous(staging, &self.config.install_dir) {
            let _ = fs::remove_dir_all(staging);
            // The swap itself puts the previous tree back if the new tree cannot
            // be moved in. That is a filesystem repair, not a failed-start rollback.
            let _ = saved_backup;
            let _ = self.start_installed().await;
            return Err(Fail::msg(
                500,
                format!(
                    "Deploy failed (swap failed: {err}). The previous build was left in place."
                ),
            ));
        }
        self.log(format!("swapped in {tag}"));
        if let Ok(mut slot) = self.version.lock() {
            *slot = tag.to_string();
        }
        let _ = saved_backup;

        if let Err(err) = self.boot_installed(tag, true).await {
            return Err(err);
        }
        self.log("kept previous install for a signed rollback");
        let _ = fs::remove_dir_all(failed_dir_path(&self.config.install_dir));
        Ok(())
    }

    /// Back up Saved, restart the installed build, and do not swap in a new tree.
    pub(crate) async fn restart_in_place(&self) -> Result<(), String> {
        if !self.config.install_dir.exists() {
            return Err("no server is installed".into());
        }
        match backup_saved(&self.config.install_dir, &self.config.backup_dir) {
            Ok(Some(path)) => {
                self.log(format!("backed up Saved to {}", path.display()));
                prune_backups(
                    &self.config.backup_dir,
                    self.config.auto_update.keep_backups,
                );
            }
            Ok(None) => self.log("no Saved folder to back up"),
            Err(err) => return Err(format!("save backup failed: {err}")),
        }
        let running = self.process.lock().await.is_some();
        if running {
            self.stop_for(&StopStyle::Grace)
                .await
                .map_err(|err| err.message)?;
        } else {
            self.log("server is installed but not running; starting it");
        }
        self.start_installed().await.map_err(|err| err.message)?;
        self.health_check().await?;
        Ok(())
    }

    /// Start the installed build. On a failed health check, save logs and dumps
    /// first, stay on this build, and do not restore the previous tree.
    /// Prerequisites run before the first start when `prereqs` is set, and once
    /// more if the process exits with a loader status.
    async fn boot_installed(&self, tag: &str, prereqs: bool) -> Result<(), Fail> {
        if prereqs {
            self.run_prereqs(tag, false).await;
        }
        self.set_phase("starting");
        if let Err(err) = self.start_installed().await {
            let snap = self.capture_failure(tag, &err.message);
            self.latch_failed_start(tag, snap);
            return Err(Fail::msg(500, stay_message(tag, &err.message)));
        }
        if let Err(err) = self.health_check().await {
            let code = self.exit_code();
            let snap = self.capture_failure(tag, &err);
            let retried = self
                .prereq_retried
                .lock()
                .map(|slot| slot.as_str() == tag)
                .unwrap_or(true);
            if crate::prereq::should_retry(code, retried) {
                if let Ok(mut slot) = self.prereq_retried.lock() {
                    *slot = tag.to_string();
                }
                self.log(
                    "loader error; running bundled prerequisites once and retrying the start once",
                );
                self.run_prereqs(tag, true).await;
                if self.start_installed().await.is_ok() && self.health_check().await.is_ok() {
                    self.clear_failed_start();
                    return Ok(());
                }
                let snap = self.capture_failure(tag, "retry after prerequisites failed");
                self.latch_failed_start(tag, snap);
                return Err(Fail::msg(
                    500,
                    stay_message(tag, "retry after prerequisites failed"),
                ));
            }
            self.latch_failed_start(tag, snap);
            return Err(Fail::msg(500, stay_message(tag, &err)));
        }
        self.clear_failed_start();
        Ok(())
    }

    async fn run_prereqs(&self, tag: &str, force: bool) {
        match crate::prereq::install_bundled(
            &self.config.install_dir,
            &self.config.work_dir,
            tag,
            force,
        ) {
            Ok(note) if !note.is_empty() => self.log(note),
            Ok(_) => {}
            Err(err) => self.log(format!("prerequisites not installed: {err}")),
        }
    }

    fn capture_failure(&self, tag: &str, reason: &str) -> crate::diag::FailureSnapshot {
        crate::diag::snapshot_failure(
            &self.config.work_dir,
            &self.config.install_dir,
            &self.config.log_dir,
            self.server_log_mark.load(Ordering::Acquire),
            tag,
            reason,
            self.exit_code(),
        )
    }

    fn latch_failed_start(&self, tag: &str, snap: crate::diag::FailureSnapshot) {
        let report = FailedStart {
            tag: tag.to_string(),
            at: chrono::Utc::now().to_rfc3339(),
            exit_code: self.exit_code(),
            exit_text: snap.exit_text,
            log_tail: snap.log_tail,
            artifact_dir: snap.relative_dir.clone(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&report) {
            let _ = fs::create_dir_all(&self.config.work_dir);
            let _ = fs::write(self.config.work_dir.join("failed-start.json"), text);
        }
        if let Ok(mut slot) = self.failed_start.lock() {
            *slot = Some(report.clone());
        }
        self.expect_running.store(false, Ordering::Release);
        self.set_phase("failed-start");
        self.record_event(
            "failed-start",
            format!(
                "staying on {tag} ({}); evidence in {}",
                report.exit_text, snap.relative_dir
            ),
        );
        self.log(format!(
            "failed start of {tag}; staying on this build; evidence in {}",
            snap.relative_dir
        ));
    }

    fn clear_failed_start(&self) {
        if let Ok(mut slot) = self.failed_start.lock() {
            *slot = None;
        }
        let _ = fs::remove_file(self.config.work_dir.join("failed-start.json"));
    }

    /// Signed manual rollback. Restores `{install}.previous` and starts it.
    /// Never called from a failed health check.
    pub async fn rollback_previous(&self) -> Result<AgentStatus, Fail> {
        let _guard = self.begin_real()?;
        let install = self.config.install_dir.clone();
        let previous =
            previous_install_dir(&install).map_err(|err| Fail::msg(500, err.to_string()))?;
        if !previous.is_dir() {
            return Err(Fail::msg(404, "No previous game build is on disk."));
        }
        self.log("manual rollback requested");
        self.set_phase("rolling-back");
        let result = async {
            self.stop_running(true).await?;
            restore_previous(&install).map_err(|err| Fail::msg(500, err.to_string()))?;
            let restored = read_version(&install);
            if let Ok(mut slot) = self.version.lock() {
                *slot = restored.clone();
            }
            self.log(format!("manual rollback restored {restored}"));
            self.record_event("manual-rollback", format!("restored {restored}"));
            self.boot_installed(&restored, false).await?;
            Ok::<(), Fail>(())
        }
        .await;
        match &result {
            Ok(()) => self.set_phase("idle"),
            Err(_) => self.keep_failed_phase(),
        }
        result?;
        Ok(self.status().await)
    }

    async fn stop_for(&self, style: &StopStyle) -> Result<(), Fail> {
        match style {
            StopStyle::Warn => self.stop_running(true).await,
            StopStyle::Grace => self.stop_for_maintenance().await,
        }
    }

    async fn stop_for_maintenance(&self) -> Result<(), Fail> {
        let running = self.process.lock().await.is_some();
        if !running {
            self.online.store(false, Ordering::Release);
            self.log("no server process to stop");
            return Ok(());
        }
        if let Some(count) = player_count(&self.config.install_dir) {
            if count > 0 {
                let grace = self.config.auto_update.grace_seconds;
                let line = format!(
                    "say uXa: this server will restart when the match is empty (up to {grace} seconds). Leave and rejoin after it is back."
                );
                match self.write_stdin(&line).await {
                    Ok(()) => self.log("broadcast warning"),
                    Err(err) => self.log(format!("warning broadcast unavailable: {err}")),
                }
                let started = Instant::now();
                loop {
                    match player_count(&self.config.install_dir) {
                        Some(0) | None => {
                            self.log("players have left");
                            break;
                        }
                        Some(_) if started.elapsed() >= Duration::from_secs(grace) => {
                            self.log(format!("grace period of {grace}s elapsed"));
                            break;
                        }
                        _ => sleep(Duration::from_millis(100)).await,
                    }
                }
                return self.request_quit().await;
            }
        }
        self.stop_running(true).await
    }

    async fn stop_running(&self, warn: bool) -> Result<(), Fail> {
        let running = {
            let guard = self.process.lock().await;
            guard.is_some()
        };
        if !running {
            self.online.store(false, Ordering::Release);
            self.log("no server process to stop");
            return Ok(());
        }
        if warn {
            let line = format!(
                "say uXa: this server will restart in {} seconds. Leave the match and rejoin after it is back.",
                self.config.warn_seconds
            );
            match self.write_stdin(&line).await {
                Ok(()) => self.log("broadcast warning"),
                Err(err) => self.log(format!("warning broadcast unavailable: {err}")),
            }
            if self.config.warn_seconds > 0 {
                sleep(Duration::from_secs(self.config.warn_seconds)).await;
            }
            return self.request_quit().await;
        }
        self.kill_running().await?;
        self.log("server process stopped");
        Ok(())
    }

    async fn request_quit(&self) -> Result<(), Fail> {
        match self.write_stdin("quit").await {
            Ok(()) => self.log("sent quit"),
            Err(err) => self.log(format!("quit command failed: {err}")),
        }
        let exited = self.wait_exit(self.config.quit_wait_secs).await;
        if exited {
            self.mark_offline();
            self.log("server process exited");
            return Ok(());
        }
        self.log("server did not exit after quit; stopping the process");
        self.kill_running().await?;
        self.log("server process stopped");
        Ok(())
    }

    async fn kill_running(&self) -> Result<(), Fail> {
        let mut guard = self.process.lock().await;
        if let Some(mut running) = guard.take() {
            // The UE dedicated server is a bootstrap exe that spawns Binaries\Win64\...Server.exe.
            // Killing only the bootstrap leaves the real server running and its files locked.
            kill_pid(running.pid);
            if let Some(mut child) = running.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Some(mut bridge) = running.bridge.take() {
                let _ = bridge.kill();
                let _ = bridge.wait();
            } else if let Some(pid) = running.bridge_pid {
                kill_pid(pid);
            }
        }
        drop(guard);
        self.mark_offline();
        Ok(())
    }

    async fn write_stdin(&self, line: &str) -> Result<(), String> {
        let mut guard = self.process.lock().await;
        let Some(running) = guard.as_mut() else {
            return Err("no process".into());
        };
        if let Some(path) = running.command_file.clone() {
            drop(guard);
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|err| err.to_string())?;
            writeln!(file, "{line}").map_err(|err| err.to_string())?;
            file.flush().map_err(|err| err.to_string())?;
            return Ok(());
        }
        let Some(stdin) = running.stdin.as_mut() else {
            return Err("process has no stdin".into());
        };
        writeln!(stdin, "{line}").map_err(|err| err.to_string())?;
        stdin.flush().map_err(|err| err.to_string())?;
        Ok(())
    }

    async fn wait_exit(&self, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let mut guard = self.process.lock().await;
            let Some(running) = guard.as_mut() else {
                return true;
            };
            let exited = if let Some(child) = running.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => true,
                    Ok(None) => false,
                }
            } else {
                !pid_alive(running.pid)
            };
            if exited {
                *guard = None;
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            drop(guard);
            sleep(Duration::from_millis(50)).await;
        }
    }

    async fn start_installed(&self) -> Result<(), Fail> {
        if self.process.lock().await.is_some() {
            return Err(Fail::msg(409, "server process is already running"));
        }
        let exe = locate_executable(&self.config.install_dir, &self.config.executable).ok_or_else(
            || {
                Fail::msg(
                    500,
                    format!(
                        "could not find {} under {}",
                        self.config.executable,
                        self.config.install_dir.display()
                    ),
                )
            },
        )?;
        make_executable(&exe);
        let log_path = self.config.log_dir.join("server.log");
        let mark = fs::metadata(&log_path).map(|meta| meta.len()).unwrap_or(0);
        self.server_log_mark.store(mark, Ordering::Release);
        if let Ok(mut slot) = self.last_exit.lock() {
            *slot = None;
        }
        let args = launch_args(&self.config);
        let cwd = exe.parent().unwrap_or(self.config.install_dir.as_path());
        self.log(format!("starting {} {}", exe.display(), args.join(" ")));

        match spawn_bridge(&self.config.work_dir) {
            Ok(mut bridge) => {
                let stdin = bridge.server_stdin.take().unwrap_or(Stdio::null());
                let (stdout, stderr) = server_logs(&self.config.log_dir);
                match spawn_server(&exe, &args, cwd, stdin, stdout, stderr) {
                    Ok(child) => {
                        let pid = child.id();
                        let command_file = bridge.command_file.clone();
                        let bridge_pid = bridge.pid;
                        self.write_process_record(
                            pid,
                            Some(bridge_pid),
                            Some(command_file.as_path()),
                        );
                        let running = Running {
                            child: Some(child),
                            stdin: None,
                            bridge: Some(bridge.child),
                            command_file: Some(command_file),
                            pid,
                            bridge_pid: Some(bridge_pid),
                        };
                        *self.process.lock().await = Some(running);
                        self.mark_started();
                        return Ok(());
                    }
                    Err(err) => {
                        let _ = bridge.child.kill();
                        let _ = bridge.child.wait();
                        return Err(Fail::msg(
                            500,
                            format!("could not start {}: {err}", exe.display()),
                        ));
                    }
                }
            }
            Err(err) => {
                self.log(format!(
                    "stdin bridge unavailable ({err}); using a direct pipe"
                ));
            }
        }

        let (stdout, stderr) = server_logs(&self.config.log_dir);
        let mut command = Command::new(&exe);
        command
            .args(&args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(stderr);
        detach(&mut command);
        let mut child = command
            .spawn()
            .map_err(|err| Fail::msg(500, format!("could not start {}: {err}", exe.display())))?;
        let pid = child.id();
        let stdin = child.stdin.take();
        self.write_process_record(pid, None, None);
        *self.process.lock().await = Some(Running {
            child: Some(child),
            stdin,
            bridge: None,
            command_file: None,
            pid,
            bridge_pid: None,
        });
        self.mark_started();
        Ok(())
    }

    fn mark_started(&self) {
        self.expect_running.store(true, Ordering::Release);
        self.record_event(
            "start",
            format!(
                "game server started ({})",
                read_version(&self.config.install_dir)
            ),
        );
        if let Ok(mut slot) = self.started_at.lock() {
            *slot = Some(Instant::now());
        }
        let version = read_version(&self.config.install_dir);
        if !version.is_empty() {
            if let Ok(mut slot) = self.version.lock() {
                *slot = version;
            }
        }
        self.online.store(true, Ordering::Release);
    }

    fn write_process_record(&self, pid: u32, bridge_pid: Option<u32>, command_file: Option<&Path>) {
        let path = self.config.work_dir.join("server-process.json");
        let record = ProcessRecord {
            pid,
            bridge_pid,
            command_file: command_file
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        };
        if let Ok(text) = serde_json::to_string(&record) {
            let _ = fs::create_dir_all(&self.config.work_dir);
            let _ = fs::write(path, text);
        }
    }

    async fn health_check(&self) -> Result<(), String> {
        sleep(Duration::from_secs(self.config.health_wait_secs)).await;
        self.refresh_liveness().await;
        if !self.online.load(Ordering::Acquire) {
            return Err("process exited before it was healthy".into());
        }
        if self.config.require_tcp {
            let addr = format!("127.0.0.1:{}", self.config.port);
            match tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr)).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    return Err(format!(
                        "port {} is not accepting connections: {err}",
                        self.config.port
                    ))
                }
                Err(_) => return Err(format!("timed out connecting to {}", self.config.port)),
            }
            self.log(format!("tcp health check passed on {}", self.config.port));
        }
        self.log("health check passed");
        Ok(())
    }

    async fn refresh_liveness(&self) {
        let mut guard = self.process.lock().await;
        if let Some(running) = guard.as_mut() {
            let (alive, code) = if let Some(child) = running.child.as_mut() {
                match child.try_wait() {
                    Ok(None) => (true, None),
                    Ok(Some(status)) => (false, status.code()),
                    Err(_) => (false, None),
                }
            } else {
                (pid_alive(running.pid), None)
            };
            if alive {
                self.online.store(true, Ordering::Release);
            } else {
                if let Some(code) = code {
                    if let Ok(mut slot) = self.last_exit.lock() {
                        *slot = Some(code);
                    }
                }
                if let Some(mut bridge) = running.bridge.take() {
                    let _ = bridge.kill();
                    let _ = bridge.wait();
                } else if let Some(pid) = running.bridge_pid {
                    kill_pid(pid);
                }
                *guard = None;
                drop(guard);
                if self.expect_running.load(Ordering::Acquire) {
                    let phase = self.phase.lock().map(|p| p.clone()).unwrap_or_default();
                    self.record_event(
                        "exit",
                        format!("game server process exited (phase {phase})"),
                    );
                }
                self.mark_offline();
                return;
            }
        } else {
            self.online.store(false, Ordering::Release);
        }
    }

    fn mark_offline(&self) {
        self.online.store(false, Ordering::Release);
        if let Ok(mut slot) = self.started_at.lock() {
            *slot = None;
        }
        let _ = fs::remove_file(self.config.work_dir.join("server-process.json"));
    }
}

pub(crate) struct ActionGuard<'a> {
    agent: &'a Agent,
}

impl Drop for ActionGuard<'_> {
    fn drop(&mut self) {
        self.agent.busy.store(false, Ordering::Release);
    }
}

fn launch_args(config: &AgentConfig) -> Vec<String> {
    let mut args = config.args.clone();
    let mentions_port = args
        .iter()
        .any(|arg| arg.to_ascii_lowercase().contains("port"));
    if !mentions_port {
        args.push(format!("-port={}", config.port));
    }
    args
}

fn file_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("server.zip")
        .to_string()
}

fn staging_dir(install: &Path) -> Result<PathBuf, Fail> {
    let parent = install
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Fail::msg(500, "install path has no parent"))?;
    let name = install
        .file_name()
        .ok_or_else(|| Fail::msg(500, "install path has no name"))?;
    Ok(parent.join(format!("{}.staging", name.to_string_lossy())))
}

fn failed_dir_path(install: &Path) -> PathBuf {
    install.parent().unwrap_or(Path::new(".")).join(format!(
        "{}.failed",
        install
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("install")
    ))
}

fn write_version(dir: &Path, tag: &str) -> std::io::Result<()> {
    fs::write(dir.join(VERSION_FILE), format!("{tag}\n"))
}

fn read_version(dir: &Path) -> String {
    fs::read_to_string(dir.join(VERSION_FILE))
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn previous_version(install: &Path) -> String {
    previous_install_dir(install)
        .ok()
        .filter(|path| path.is_dir())
        .map(|path| read_version(&path))
        .unwrap_or_default()
}

fn stay_message(tag: &str, reason: &str) -> String {
    format!(
        "Staying on {tag} after a failed start ({reason}). Previous build kept for POST /v1/rollback."
    )
}

fn load_persisted_failure(work: &Path) -> Option<FailedStart> {
    let text = fs::read_to_string(work.join("failed-start.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn player_count(install: &Path) -> Option<u32> {
    crate::monitor::read_player_status(install).and_then(|p| p.count)
}

/// Copy every Saved folder of the install (root `Saved` and the Unreal project's
/// `<Project>/Saved`, where accounts and characters live) to `<backup_root>/<UTC stamp>`.
fn backup_saved(install: &Path, backup_root: &Path) -> Result<Option<PathBuf>, String> {
    let found = launcher_core::resolve_preserved(install, Path::new("Saved"));
    if found.is_empty() {
        return Ok(None);
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let dest = backup_root.join(format!("{stamp}"));
    fs::create_dir_all(&dest).map_err(|err| err.to_string())?;
    copy_preserved(install, &dest, &["Saved".into()]).map_err(|err| err.to_string())?;
    Ok(Some(dest))
}

fn modified(path: &Path) -> std::time::SystemTime {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

fn stamp_of(path: &Path) -> String {
    chrono::DateTime::<chrono::Utc>::from(modified(path)).to_rfc3339()
}

fn newest_file(dir: &Path, ext: Option<&str>) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .filter(|p| {
            ext.map_or(true, |x| {
                p.extension().map_or(false, |e| e.eq_ignore_ascii_case(x))
            })
        })
        .max_by_key(|p| modified(p))
}

fn tail_lines(path: &Path, lines: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(256 * 1024);
    let _ = file.seek(SeekFrom::Start(start));
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn log_entry(path: &Path, lines: usize) -> serde_json::Value {
    serde_json::json!({
        "file": path.display().to_string(),
        "modified": stamp_of(path),
        "tail": crate::diag::redact(&tail_lines(path, lines)),
    })
}

/// Remove `SaveGames` under every Saved tree of the install. Config and logs stay.
fn wipe_save_games(install: &Path) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    for rel in launcher_core::resolve_preserved(install, Path::new("Saved")) {
        let saved = install.join(&rel);
        let games = saved.join("SaveGames");
        if games.is_dir() {
            fs::remove_dir_all(&games).map_err(|err| format!("{}: {err}", games.display()))?;
            fs::create_dir_all(&games).map_err(|err| err.to_string())?;
            removed.push(games);
        }
    }
    Ok(removed)
}

fn locate_executable(install: &Path, name: &str) -> Option<PathBuf> {
    let direct = install.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    fn walk(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
        if depth == 0 {
            return None;
        }
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_file() && path.file_name().and_then(|s| s.to_str()) == Some(name) {
                return Some(path);
            }
            if path.is_dir() {
                let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                if fname == "Saved" || fname.starts_with('.') {
                    continue;
                }
                if let Some(found) = walk(&path, name, depth - 1) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(install, name, 4)
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perm = meta.permissions();
            perm.set_mode(perm.mode() | 0o755);
            let _ = fs::set_permissions(path, perm);
        }
    }
    let _ = path;
}

const BRIDGE_PY: &str = r#"import sys, time
path = sys.argv[1]
seen = 0
while True:
    try:
        with open(path, "rb") as handle:
            handle.seek(seen)
            data = handle.read()
            if data:
                sys.stdout.buffer.write(data)
                sys.stdout.buffer.flush()
                seen += len(data)
    except FileNotFoundError:
        pass
    except Exception:
        pass
    time.sleep(0.05)
"#;

struct BridgeLaunch {
    child: Child,
    pid: u32,
    command_file: PathBuf,
    server_stdin: Option<Stdio>,
}

fn spawn_bridge(work_dir: &Path) -> Result<BridgeLaunch, String> {
    fs::create_dir_all(work_dir).map_err(|err| err.to_string())?;
    let command_file = work_dir.join("server-stdin.log");
    fs::File::create(&command_file).map_err(|err| err.to_string())?;
    let (read, write) = os_pipe::pipe().map_err(|err| err.to_string())?;
    let mut command = bridge_command(work_dir, &command_file)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(write))
        .stderr(Stdio::null());
    detach(&mut command);
    let child = command.spawn().map_err(|err| err.to_string())?;
    let pid = child.id();
    Ok(BridgeLaunch {
        child,
        pid,
        command_file,
        server_stdin: Some(Stdio::from(read)),
    })
}

fn bridge_command(work_dir: &Path, command_file: &Path) -> Result<Command, String> {
    #[cfg(windows)]
    {
        let script = work_dir.join("stdin-bridge.ps1");
        let body = format!(
            "$path = '{}'\n$seen = 0\n$out = New-Object System.IO.StreamWriter([Console]::OpenStandardOutput())\n$out.AutoFlush = $true\nwhile ($true) {{\n  if (Test-Path -LiteralPath $path) {{\n    $fs = [System.IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')\n    [void]$fs.Seek($seen, 'Begin')\n    $buf = New-Object byte[] 65536\n    $read = $fs.Read($buf, 0, $buf.Length)\n    $seen = $fs.Position\n    $fs.Close()\n    if ($read -gt 0) {{ $out.Write([System.Text.Encoding]::UTF8.GetString($buf, 0, $read)) }}\n  }}\n  Start-Sleep -Milliseconds 50\n}}\n",
            command_file.display().to_string().replace('\'', "''")
        );
        fs::write(&script, body).map_err(|err| err.to_string())?;
        let script_arg = script.display().to_string();
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            &script_arg,
        ]);
        Ok(command)
    }
    #[cfg(not(windows))]
    {
        let script = work_dir.join("stdin-bridge.py");
        fs::write(&script, BRIDGE_PY).map_err(|err| err.to_string())?;
        let mut command = Command::new("python3");
        command.arg("-u").arg(&script).arg(command_file);
        Ok(command)
    }
}

fn spawn_server(
    exe: &Path,
    args: &[String],
    cwd: &Path,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
) -> Result<Child, String> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr);
    detach(&mut command);
    command.spawn().map_err(|err| err.to_string())
}

fn server_logs(log_dir: &Path) -> (Stdio, Stdio) {
    let _ = fs::create_dir_all(log_dir);
    let path = log_dir.join("server.log");
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => match file.try_clone() {
            Ok(err_file) => (Stdio::from(file), Stdio::from(err_file)),
            Err(_) => (Stdio::from(file), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    }
}

fn detach(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
}

fn load_running(config: &AgentConfig) -> Option<Running> {
    let path = config.work_dir.join("server-process.json");
    let text = fs::read_to_string(&path).ok()?;
    let record: ProcessRecord = serde_json::from_str(&text).ok()?;
    if record.pid == 0 || !pid_alive(record.pid) {
        if let Some(bridge) = record.bridge_pid {
            kill_pid(bridge);
        }
        let _ = fs::remove_file(&path);
        return None;
    }
    Some(Running {
        child: None,
        stdin: None,
        bridge: None,
        command_file: if record.command_file.is_empty() {
            None
        } else {
            Some(PathBuf::from(record.command_file))
        },
        pid: record.pid,
        bridge_pid: record.bridge_pid,
    })
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(windows)]
    {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

fn kill_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Remove each live preserved path from `staging`, then copy the live tree.
/// A zip that ships its own Saved cannot replace or remain beside the live one.
/// Paths that do not exist in the live install are left as the zip extracted them.
fn overlay_preserved(install: &Path, staging: &Path, relatives: &[String]) -> Result<(), String> {
    for relative in relatives {
        let rel = Path::new(relative.trim());
        if relative.trim().is_empty() {
            continue;
        }
        for found in resolve_preserved(install, rel) {
            let dst = staging.join(&found);
            if dst.is_dir() {
                fs::remove_dir_all(&dst).map_err(|err| err.to_string())?;
            } else if dst.exists() {
                fs::remove_file(&dst).map_err(|err| err.to_string())?;
            }
        }
    }
    copy_preserved(install, staging, relatives).map_err(|err| err.to_string())
}

fn restore_saved_backup(install: &Path, backup: &Path) -> Result<(), String> {
    let found = resolve_preserved(backup, Path::new("Saved"));
    if found.is_empty() {
        return Ok(());
    }
    for rel in &found {
        let live = install.join(rel);
        if live.is_dir() {
            fs::remove_dir_all(&live).map_err(|err| err.to_string())?;
        } else if live.exists() {
            fs::remove_file(&live).map_err(|err| err.to_string())?;
        }
    }
    copy_preserved(backup, install, &["Saved".into()]).map_err(|err| err.to_string())
}

fn prune_backups(backup_root: &Path, keep: u32) {
    if keep == 0 || !backup_root.is_dir() {
        return;
    }
    let mut stamps: Vec<PathBuf> = fs::read_dir(backup_root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_backup_stamp)
        })
        .collect();
    stamps.sort();
    let keep = keep as usize;
    if stamps.len() <= keep {
        return;
    }
    let extra = stamps.len() - keep;
    for path in stamps.into_iter().take(extra) {
        let _ = fs::remove_dir_all(path);
    }
}

fn is_backup_stamp(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 15
        && bytes[8] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 8 || byte.is_ascii_digit())
}

#[cfg(test)]
mod preserve_tests {
    use super::*;

    #[test]
    fn overlay_drops_packaged_saved_and_leaves_the_live_tree() {
        let temp = tempfile::tempdir().unwrap();
        let live = temp.path().join("install");
        let staging = temp.path().join("staging");
        fs::create_dir_all(live.join("FridayNightFights/Saved")).unwrap();
        fs::write(live.join("FridayNightFights/Saved/world.sav"), b"WORLD").unwrap();
        fs::write(live.join("FridayNightFights/Saved/account.txt"), b"ACCOUNT").unwrap();
        fs::create_dir_all(staging.join("FridayNightFights/Saved")).unwrap();
        fs::write(staging.join("FridayNightFights/Saved/wiped.txt"), b"WIPED").unwrap();
        overlay_preserved(&live, &staging, &["Saved".into()]).unwrap();
        assert_eq!(
            fs::read(live.join("FridayNightFights/Saved/world.sav")).unwrap(),
            b"WORLD"
        );
        assert_eq!(
            fs::read(staging.join("FridayNightFights/Saved/world.sav")).unwrap(),
            b"WORLD"
        );
        assert_eq!(
            fs::read(staging.join("FridayNightFights/Saved/account.txt")).unwrap(),
            b"ACCOUNT"
        );
        assert!(!staging.join("FridayNightFights/Saved/wiped.txt").exists());
    }

    #[test]
    fn saved_backup_replaces_a_corrupt_tree() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("install");
        let backup = temp.path().join("backup");
        fs::create_dir_all(install.join("FridayNightFights/Saved")).unwrap();
        fs::write(
            install.join("FridayNightFights/Saved/world.sav"),
            b"CORRUPT",
        )
        .unwrap();
        fs::create_dir_all(backup.join("FridayNightFights/Saved")).unwrap();
        fs::write(backup.join("FridayNightFights/Saved/world.sav"), b"WORLD").unwrap();
        restore_saved_backup(&install, &backup).unwrap();
        assert_eq!(
            fs::read(install.join("FridayNightFights/Saved/world.sav")).unwrap(),
            b"WORLD"
        );
    }

    #[test]
    fn wipe_clears_save_games_and_keeps_config() {
        let root = std::env::temp_dir().join(format!("uxa-wipe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let sg = root.join("FridayNightFights/Saved/SaveGames");
        fs::create_dir_all(&sg).unwrap();
        fs::write(sg.join("FNF_Account.sav"), b"acct").unwrap();
        let cfg = root.join("FridayNightFights/Saved/Config");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(cfg.join("Game.ini"), b"cfg").unwrap();
        let removed = wipe_save_games(&root).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(sg.is_dir());
        assert!(!sg.join("FNF_Account.sav").exists());
        assert!(cfg.join("Game.ini").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn prune_keeps_the_newest_timestamped_backups() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "20260101-000000",
            "20260102-000000",
            "20260103-000000",
            "notes",
        ] {
            fs::create_dir_all(temp.path().join(name)).unwrap();
        }
        prune_backups(temp.path(), 0);
        assert!(temp.path().join("20260101-000000").is_dir());
        prune_backups(temp.path(), 1);
        assert!(!temp.path().join("20260101-000000").exists());
        assert!(!temp.path().join("20260102-000000").exists());
        assert!(temp.path().join("20260103-000000").is_dir());
        assert!(temp.path().join("notes").is_dir());
    }
}

pub(crate) fn detach_command(command: &mut Command) {
    detach(command);
}

pub(crate) fn kill_pid_pub(pid: u32) {
    kill_pid(pid);
}

pub(crate) fn tail_text(path: &Path, lines: usize) -> String {
    tail_lines(path, lines)
}

pub(crate) fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

impl Agent {
    /// Put a Saved backup (a folder name from GET /v1/monitor "backups") back in place.
    /// The current Saved is backed up first, so a restore can itself be undone.
    pub async fn restore_backup(&self, name: &str) -> Result<AgentStatus, Fail> {
        let name = name.trim();
        if !is_backup_stamp(name) {
            return Err(Fail::msg(
                400,
                "backup must be a YYYYMMDD-HHMMSS folder name from the backup list",
            ));
        }
        let backup = self.config.backup_dir.join(name);
        if !backup.is_dir() {
            return Err(Fail::msg(404, format!("no backup named {name}")));
        }
        let _guard = self.begin_real()?;
        self.log(format!("restore of backup {name} requested"));
        self.set_phase("stopping");
        let result = async {
            self.stop_running(true).await?;
            self.set_phase("backing-up");
            let before =
                backup_saved(&self.config.install_dir, &self.config.backup_dir).map_err(|e| {
                    Fail::msg(
                        500,
                        format!("could not back up the current Saved first: {e}"),
                    )
                })?;
            if let Some(path) = &before {
                self.log(format!("current Saved backed up to {}", path.display()));
            }
            self.set_phase("restoring");
            restore_saved_backup(&self.config.install_dir, &backup)
                .map_err(|e| Fail::msg(500, format!("restore failed: {e}")))?;
            self.set_phase("starting");
            self.start_installed().await?;
            self.health_check().await.map_err(|e| {
                Fail::msg(
                    500,
                    format!("restored, but the server failed its health check: {e}"),
                )
            })?;
            Ok::<(), Fail>(())
        }
        .await;
        self.set_phase("idle");
        match &result {
            Ok(()) => self.record_event("restore", format!("Saved restored from backup {name}")),
            Err(e) => self.record_event("restore-failed", e.message.clone()),
        }
        result?;
        Ok(self.status().await)
    }

    pub(crate) async fn server_pid(&self) -> Option<u32> {
        self.process.lock().await.as_ref().map(|r| r.pid)
    }

    /// Persistent event history (starts, exits, crashes, deploys, rollbacks, wipes).
    pub(crate) fn record_event(&self, kind: &str, message: impl Into<String>) {
        let line = serde_json::json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "kind": kind,
            "message": message.into(),
        });
        let _ = fs::create_dir_all(&self.config.work_dir);
        let path = self.config.work_dir.join("events.jsonl");
        if fs::metadata(&path)
            .map(|m| m.len() > 2_000_000)
            .unwrap_or(false)
        {
            let _ = fs::rename(&path, self.config.work_dir.join("events.old.jsonl"));
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(file, "{line}");
        }
    }

    pub(crate) fn recent_events(&self, max: usize) -> Vec<serde_json::Value> {
        let text =
            fs::read_to_string(self.config.work_dir.join("events.jsonl")).unwrap_or_default();
        let mut out: Vec<serde_json::Value> = text
            .lines()
            .rev()
            .filter_map(|l| serde_json::from_str(l).ok())
            .take(max)
            .collect();
        out.reverse();
        out
    }

    /// Restart a game server that died on its own. At most 3 restarts an hour.
    pub(crate) async fn game_watchdog(&self) {
        if !self.expect_running.load(Ordering::Acquire) || self.busy.load(Ordering::Acquire) {
            return;
        }
        self.refresh_liveness().await;
        if self.online.load(Ordering::Acquire) || !self.config.install_dir.exists() {
            return;
        }
        let Ok(_guard) = self.begin_real() else {
            return;
        };
        let recent = {
            let mut list = self.crash_restarts.lock().unwrap();
            list.retain(|t| t.elapsed() < Duration::from_secs(3600));
            list.push(Instant::now());
            list.len()
        };
        if recent > 3 {
            self.expect_running.store(false, Ordering::Release);
            let tag = read_version(&self.config.install_dir);
            let snap = self.capture_failure(&tag, "crash loop; not restarting");
            self.latch_failed_start(&tag, snap);
            self.record_event(
                "crash-loop",
                "game server crashed 3 times within an hour; watchdog stopped restarting it",
            );
            self.log("watchdog: crash loop, not restarting the game server again");
            return;
        }
        let tag = read_version(&self.config.install_dir);
        let _ = self.capture_failure(&tag, "game server process exited");
        self.record_event(
            "crash-restart",
            "game server was down; watchdog restarting it",
        );
        self.log("watchdog: game server is down; restarting it");
        self.set_phase("starting");
        let started = self.start_installed().await;
        let result = match started {
            Ok(()) => self.health_check().await.map_err(|e| e),
            Err(e) => Err(e.message),
        };
        self.set_phase("idle");
        if let Err(e) = result {
            self.record_event("crash-restart-failed", e);
        }
    }
}
