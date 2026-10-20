//! Download, swap, start, and roll back the dedicated server.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use launcher_core::{
    copy_preserved, discard_previous, download_url, download_with_resume, extract_zip,
    find_release_by_tag, http_client, list_matching_releases, previous_install_dir,
    resolve_preserved, restore_previous, swap_keeping_previous, verify_download, AgentLogLine,
    AgentReleaseSummary, AgentStatus, DownloadRequest, ReleaseInfo, ReleaseSource,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::config::AgentConfig;
use crate::schedule::{format_local, next_restart, parse_clock, parse_zone};
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
    busy: AtomicBool,
    phase: Mutex<String>,
    started_at: Mutex<Option<Instant>>,
    version: Mutex<String>,
    online: AtomicBool,
    pub(crate) updates: Mutex<UpdateState>,
    pub(crate) scheduler_started: AtomicBool,
}

#[derive(Debug)]
pub struct Fail {
    pub http: u16,
    pub message: String,
}

impl Fail {
    fn msg(http: u16, message: impl Into<String>) -> Self {
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
        Ok(Arc::new(Self {
            updates: Mutex::new(load_state(&config.work_dir)),
            scheduler_started: AtomicBool::new(false),
            config,
            client,
            process: tokio::sync::Mutex::new(adopted),
            logs: Mutex::new(std::collections::VecDeque::new()),
            next_log: AtomicU64::new(1),
            busy: AtomicBool::new(false),
            phase: Mutex::new("idle".into()),
            started_at: Mutex::new(if online { Some(Instant::now()) } else { None }),
            version: Mutex::new(version),
            online: AtomicBool::new(online),
        }))
    }

    pub fn log(&self, message: impl Into<String>) {
        let message = message.into();
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
            next_restart_at: self.next_restart_label(),
        }
    }

    fn next_restart_label(&self) -> String {
        if !self.config.auto_update.enabled {
            return String::new();
        }
        let Some(time) = parse_clock(&self.config.auto_update.restart_time) else {
            return String::new();
        };
        let Ok(tz) = parse_zone(&self.config.auto_update.restart_time_zone) else {
            return String::new();
        };
        format_local(next_restart(chrono::Utc::now(), time, tz), tz)
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
        self.set_phase("idle");
        match result {
            Ok(()) => {
                let status = self.status().await;
                self.log(format!(
                    "deploy finished version={} online={}",
                    status.version, status.online
                ));
                Ok(status)
            }
            Err(err) => {
                self.log(format!("deploy failed: {}", err.message));
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
        if let Err(err) = self.start_installed().await {
            self.set_phase("idle");
            self.log(format!("restart failed: {}", err.message));
            return Err(err);
        }
        if let Err(err) = self.health_check().await {
            self.set_phase("idle");
            let message = format!("Restart failed health check: {err}");
            self.log(&message);
            return Err(Fail::msg(500, message));
        }
        self.set_phase("idle");
        let status = self.status().await;
        self.log(format!("restart finished online={}", status.online));
        Ok(status)
    }

    pub async fn stop(&self) -> Result<AgentStatus, Fail> {
        let _guard = self.begin_real()?;
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
                self.log(format!("wipe: backup failed ({err}); nothing was deleted, restarting"));
                let _ = self.start_installed().await;
                return Err(Fail::msg(
                    500,
                    format!("Save backup failed: {err}. Nothing was wiped; the server was restarted."),
                ));
            }
        }
        self.set_phase("wiping");
        let removed = wipe_save_games(&self.config.install_dir).map_err(|err| {
            Fail::msg(500, format!("wipe: could not clear SaveGames: {err}"))
        });
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
        self.health_check()
            .await
            .map_err(|err| Fail::msg(500, format!("Wiped, but the restart failed its health check: {err}")))?;
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
        ReleaseSource {
            api_base: self.config.api_base.clone(),
            owner: self.config.owner.clone(),
            repo: self.config.repo.clone(),
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
            let reason = format!("swap failed: {err}");
            if let Err(rollback_err) = self.rollback(&reason, saved_backup.as_deref()).await {
                return Err(Fail::msg(
                    500,
                    format!(
                        "Deploy failed ({reason}) and rollback failed: {}",
                        rollback_err.message
                    ),
                ));
            }
            return Err(Fail::msg(
                500,
                format!("Deploy failed ({reason}). Previous build restored."),
            ));
        }
        self.log(format!("swapped in {tag}"));
        if let Ok(mut slot) = self.version.lock() {
            *slot = tag.to_string();
        }

        self.set_phase("starting");
        if let Err(err) = self.start_installed().await {
            self.rollback(&err.message, saved_backup.as_deref()).await?;
            return Err(Fail::msg(
                500,
                format!("Deploy failed ({}). Previous build restored.", err.message),
            ));
        }
        if let Err(err) = self.health_check().await {
            let reason = err;
            if let Err(rollback_err) = self.rollback(&reason, saved_backup.as_deref()).await {
                return Err(Fail::msg(
                    500,
                    format!(
                        "Deploy failed ({reason}) and rollback failed: {}",
                        rollback_err.message
                    ),
                ));
            }
            return Err(Fail::msg(
                500,
                format!("Deploy failed ({reason}). Previous build restored."),
            ));
        }
        if let Err(err) = discard_previous(&self.config.install_dir) {
            self.log(format!(
                "online, but the previous copy could not be removed: {err}"
            ));
        } else {
            self.log("removed previous install");
        }
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

    async fn rollback(&self, reason: &str, saved_backup: Option<&Path>) -> Result<(), Fail> {
        self.set_phase("rolling-back");
        self.log(format!("health check failed: {reason}"));
        self.log("rolling back to previous install");
        let _ = self.kill_running().await;
        let install = &self.config.install_dir;
        let previous =
            previous_install_dir(install).map_err(|err| Fail::msg(500, err.to_string()))?;
        if previous.is_dir() {
            restore_previous(install).map_err(|err| Fail::msg(500, err.to_string()))?;
            if let Some(backup) = saved_backup {
                match restore_saved_backup(install, backup) {
                    Ok(()) => self.log(format!("restored Saved from {}", backup.display())),
                    Err(err) => self.log(format!("could not restore Saved backup: {err}")),
                }
            }
            let restored = read_version(install);
            if let Ok(mut slot) = self.version.lock() {
                *slot = restored;
            }
            self.log("restored previous install");
            self.start_installed().await?;
            self.health_check().await.map_err(|err| {
                Fail::msg(500, format!("rolled-back build failed health check: {err}"))
            })?;
            self.log("restarted previous build");
            Ok(())
        } else {
            self.log("no previous install; removing the failed build");
            if install.exists() {
                let _ = fs::remove_dir_all(install);
            }
            if let Ok(mut slot) = self.version.lock() {
                slot.clear();
            }
            Err(Fail::msg(500, format!("server failed to start: {reason}")))
        }
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
            let alive = if let Some(child) = running.child.as_mut() {
                matches!(child.try_wait(), Ok(None))
            } else {
                pid_alive(running.pid)
            };
            if alive {
                self.online.store(true, Ordering::Release);
            } else {
                if let Some(mut bridge) = running.bridge.take() {
                    let _ = bridge.kill();
                    let _ = bridge.wait();
                } else if let Some(pid) = running.bridge_pid {
                    kill_pid(pid);
                }
                *guard = None;
                drop(guard);
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

fn player_count(install: &Path) -> Option<u32> {
    let text = fs::read_to_string(install.join("Saved").join("uxa-status.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("playerCount")?.as_u64().map(|n| n as u32)
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
