//! Download, swap, start, and roll back the dedicated server.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use launcher_core::{
    copy_preserved, discard_previous, download_url, download_with_resume, extract_zip,
    find_release_by_tag, http_client, list_matching_releases, previous_install_dir,
    restore_previous, swap_keeping_previous, verify_download, AgentLogLine, AgentReleaseSummary,
    AgentStatus, DownloadRequest, ReleaseSource,
};
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::config::AgentConfig;

const VERSION_FILE: &str = ".uxa-server-version";

struct Running {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
}

pub struct Agent {
    pub config: AgentConfig,
    client: reqwest::Client,
    process: tokio::sync::Mutex<Option<Running>>,
    logs: Mutex<std::collections::VecDeque<AgentLogLine>>,
    next_log: AtomicU64,
    busy: AtomicBool,
    phase: Mutex<String>,
    started_at: Mutex<Option<Instant>>,
    version: Mutex<String>,
    online: AtomicBool,
}

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
        Ok(Arc::new(Self {
            config,
            client,
            process: tokio::sync::Mutex::new(None),
            logs: Mutex::new(std::collections::VecDeque::new()),
            next_log: AtomicU64::new(1),
            busy: AtomicBool::new(false),
            phase: Mutex::new("idle".into()),
            started_at: Mutex::new(None),
            version: Mutex::new(version),
            online: AtomicBool::new(false),
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

    fn set_phase(&self, phase: &str) {
        if let Ok(mut slot) = self.phase.lock() {
            *slot = phase.to_string();
        }
    }

    pub async fn status(&self) -> AgentStatus {
        self.refresh_liveness().await;
        let online = self.online.load(Ordering::Acquire);
        let version = self
            .version
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default();
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
        }
    }

    pub async fn releases(&self) -> Result<Vec<AgentReleaseSummary>, Fail> {
        self.log("listing matching server releases");
        let source = self.release_source();
        let found = list_matching_releases(&self.client, &source)
            .await
            .map_err(|err| Fail::msg(502, err.to_string()))?;
        if found.is_empty() {
            return Err(Fail::msg(
                404,
                "No release asset matched Server and Win64.",
            ));
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

    /// The atomic flag is the real lock. BusyGuard only exists so the HTTP layer
    /// can share the type; `begin_real` clears it when the action returns.
    fn begin_real(&self) -> Result<ActionGuard<'_>, Fail> {
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Fail::msg(409, "Another admin action is already running."));
        }
        Ok(ActionGuard { agent: self })
    }

    fn release_source(&self) -> ReleaseSource {
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
        let url = download_url(&release.asset)
            .map_err(|err| Fail::msg(502, err.to_string()))?
            .to_string();
        let downloads = self.config.work_dir.join("downloads");
        fs::create_dir_all(&downloads).map_err(|err| Fail::msg(500, err.to_string()))?;
        let archive = downloads.join(format!("{}.partial", file_name(&release.asset.name)));
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
        .map_err(|err| Fail::msg(502, err.to_string()))?;
        verify_download(
            &archive,
            release.asset.size,
            release.asset.digest.as_deref(),
        )
        .map_err(|err| Fail::msg(502, err.to_string()))?;

        let staging = staging_dir(&self.config.install_dir)?;
        if staging.exists() {
            fs::remove_dir_all(&staging).map_err(|err| Fail::msg(500, err.to_string()))?;
        }
        extract_zip(&archive, &staging).map_err(|err| Fail::msg(500, err.to_string()))?;
        write_version(&staging, &release.tag).map_err(|err| Fail::msg(500, err.to_string()))?;
        self.log(format!("extracted {}", release.asset.name));

        self.set_phase("stopping");
        self.stop_running(true).await?;
        if self.config.install_dir.exists() {
            match backup_saved(&self.config.install_dir, &self.config.backup_dir) {
                Ok(Some(path)) => self.log(format!("backed up Saved to {}", path.display())),
                Ok(None) => self.log("no Saved folder to back up"),
                Err(err) => {
                    let _ = fs::remove_dir_all(&staging);
                    let _ = self.start_installed().await;
                    return Err(Fail::msg(500, format!("save backup failed: {err}. The current build was restarted.")));
                }
            }
            if let Err(err) = copy_preserved(
                &self.config.install_dir,
                &staging,
                &self.config.preserve,
            ) {
                let _ = fs::remove_dir_all(&staging);
                let _ = self.start_installed().await;
                return Err(Fail::msg(500, format!("could not preserve saves and config: {err}. The current build was restarted.")));
            }
            self.log(format!(
                "preserved {}",
                self.config.preserve.join(", ")
            ));
            write_version(&staging, &release.tag).map_err(|err| Fail::msg(500, err.to_string()))?;
        }

        self.set_phase("deploying");
        if let Err(err) = swap_keeping_previous(&staging, &self.config.install_dir) {
            let _ = fs::remove_dir_all(&staging);
            let reason = format!("swap failed: {err}");
            if let Err(rollback_err) = self.rollback(&reason).await {
                return Err(Fail::msg(500, format!("Deploy failed ({reason}) and rollback failed: {}", rollback_err.message)));
            }
            return Err(Fail::msg(500, format!("Deploy failed ({reason}). Previous build restored.")));
        }
        self.log(format!("swapped in {}", release.tag));
        if let Ok(mut slot) = self.version.lock() {
            *slot = release.tag.clone();
        }

        self.set_phase("starting");
        if let Err(err) = self.start_installed().await {
            self.rollback(&err.message).await?;
            return Err(Fail::msg(
                500,
                format!("Deploy failed ({}). Previous build restored.", err.message),
            ));
        }
        if let Err(err) = self.health_check().await {
            let reason = err;
            if let Err(rollback_err) = self.rollback(&reason).await {
                return Err(Fail::msg(
                    500,
                    format!("Deploy failed ({reason}) and rollback failed: {}", rollback_err.message),
                ));
            }
            return Err(Fail::msg(
                500,
                format!("Deploy failed ({reason}). Previous build restored."),
            ));
        }
        if let Err(err) = discard_previous(&self.config.install_dir) {
            self.log(format!("online, but the previous copy could not be removed: {err}"));
        } else {
            self.log("removed previous install");
        }
        let _ = fs::remove_dir_all(failed_dir_path(&self.config.install_dir));
        Ok(())
    }

    async fn rollback(&self, reason: &str) -> Result<(), Fail> {
        self.set_phase("rolling-back");
        self.log(format!("health check failed: {reason}"));
        self.log("rolling back to previous install");
        let _ = self.kill_running().await;
        let install = &self.config.install_dir;
        let previous = previous_install_dir(install).map_err(|err| Fail::msg(500, err.to_string()))?;
        if previous.is_dir() {
            restore_previous(install).map_err(|err| Fail::msg(500, err.to_string()))?;
            let restored = read_version(install);
            if let Ok(mut slot) = self.version.lock() {
                *slot = restored;
            }
            self.log("restored previous install");
            self.start_installed().await?;
            self.health_check()
                .await
                .map_err(|err| Fail::msg(500, format!("rolled-back build failed health check: {err}")))?;
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
        }
        // The UE server does not read stdin, so "quit" alone never reached it and every stop was a kill
        // (no shutdown save). FNF 0.9.7+ watches <Project>/Saved/FNF_Quit.request, saves every character,
        // Sanctum and the world, then exits on its own.
        match request_clean_quit(&self.config.install_dir) {
            Ok(n) if n > 0 => self.log(format!("asked the server to save and quit ({n} quit request file(s))")),
            Ok(_) => self.log("no Saved folder for a quit request; trying the console quit"),
            Err(err) => self.log(format!("quit request failed: {err}")),
        }
        match self.write_stdin("quit").await {
            Ok(()) => self.log("sent quit"),
            Err(err) => self.log(format!("quit command failed: {err}")),
        }
        let exited = self.wait_exit(self.config.quit_wait_secs).await;
        if exited {
            self.mark_offline();
            self.log("server saved and exited");
            clear_quit_requests(&self.config.install_dir);
            return Ok(());
        }
        self.log("server did not exit after the quit request; stopping the process");
        clear_quit_requests(&self.config.install_dir);
        self.kill_running().await?;
        self.log("server process stopped");
        Ok(())
    }

    async fn kill_running(&self) -> Result<(), Fail> {
        let mut guard = self.process.lock().await;
        if let Some(mut running) = guard.take() {
            // The UE dedicated server is a bootstrap exe that spawns Binaries\Win64\...Server.exe.
            // Killing only the bootstrap leaves the real server running and its files locked.
            #[cfg(windows)]
            {
                let _ = std::process::Command::new("taskkill")
                    .args(["/PID", &running.child.id().to_string(), "/T", "/F"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            let _ = running.child.kill();
            let _ = running.child.wait();
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
            match running.child.try_wait() {
                Ok(Some(_)) => {
                    *guard = None;
                    return true;
                }
                Ok(None) if Instant::now() >= deadline => return false,
                Ok(None) => {
                    drop(guard);
                    sleep(Duration::from_millis(50)).await;
                }
                Err(_) => {
                    *guard = None;
                    return true;
                }
            }
        }
    }

    async fn start_installed(&self) -> Result<(), Fail> {
        if self.process.lock().await.is_some() {
            return Err(Fail::msg(409, "server process is already running"));
        }
        let exe = locate_executable(&self.config.install_dir, &self.config.executable)
            .ok_or_else(|| {
                Fail::msg(
                    500,
                    format!(
                        "could not find {} under {}",
                        self.config.executable,
                        self.config.install_dir.display()
                    ),
                )
            })?;
        make_executable(&exe);
        let args = launch_args(&self.config);
        let cwd = exe.parent().unwrap_or(self.config.install_dir.as_path());
        self.log(format!(
            "starting {} {}",
            exe.display(),
            args.join(" ")
        ));
        let mut command = Command::new(&exe);
        command
            .args(&args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|err| {
            Fail::msg(500, format!("could not start {}: {err}", exe.display()))
        })?;
        let stdin = child.stdin.take();
        if let Some(stdout) = child.stdout.take() {
            pump_output(stdout, "server");
        }
        if let Some(stderr) = child.stderr.take() {
            pump_output(stderr, "server");
        }
        {
            let mut guard = self.process.lock().await;
            *guard = Some(Running { child, stdin });
        }
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
        Ok(())
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
                Ok(Err(err)) => return Err(format!("port {} is not accepting connections: {err}", self.config.port)),
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
            match running.child.try_wait() {
                Ok(None) => self.online.store(true, Ordering::Release),
                _ => {
                    *guard = None;
                    self.mark_offline();
                }
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
    }
}

struct ActionGuard<'a> {
    agent: &'a Agent,
}

impl Drop for ActionGuard<'_> {
    fn drop(&mut self) {
        self.agent.busy.store(false, Ordering::Release);
    }
}

fn launch_args(config: &AgentConfig) -> Vec<String> {
    let mut args = config.args.clone();
    let mentions_port = args.iter().any(|arg| arg.to_ascii_lowercase().contains("port"));
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
    install
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!(
            "{}.failed",
            install.file_name().and_then(|n| n.to_str()).unwrap_or("install")
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

/// Name of the file FNF watches for a clean stop (FFNFServerCycle::QuitRequestPath).
const QUIT_REQUEST: &str = "FNF_Quit.request";

fn saved_dirs(install: &Path) -> Vec<PathBuf> {
    launcher_core::resolve_preserved(install, Path::new("Saved"))
        .into_iter()
        .map(|rel| install.join(rel))
        .collect()
}

/// Write the quit request into every Saved folder (root and `<Project>/Saved`). Returns how many were written.
fn request_clean_quit(install: &Path) -> Result<usize, String> {
    let mut written = 0;
    for dir in saved_dirs(install) {
        fs::write(dir.join(QUIT_REQUEST), "server agent stop\n").map_err(|err| err.to_string())?;
        written += 1;
    }
    Ok(written)
}

fn clear_quit_requests(install: &Path) {
    for dir in saved_dirs(install) {
        let _ = fs::remove_file(dir.join(QUIT_REQUEST));
    }
}

fn player_count(install: &Path) -> Option<u32> {
    let path = saved_dirs(install)
        .into_iter()
        .map(|dir| dir.join("uxa-status.json"))
        .find(|p| p.exists())?;
    let text = fs::read_to_string(path).ok()?;
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

fn pump_output<R>(reader: R, label: &str)
where
    R: std::io::Read + Send + 'static,
{
    let label = label.to_string();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match lines.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = line.trim_end();
                    if !text.is_empty() {
                        tracing::info!("{label}: {text}");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod clean_quit_tests {
    use super::*;

    #[test]
    fn quit_request_lands_in_the_project_saved_folder() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path();
        fs::create_dir_all(install.join("FridayNightFights").join("Saved")).unwrap();
        assert_eq!(request_clean_quit(install).unwrap(), 1);
        assert!(install.join("FridayNightFights").join("Saved").join(QUIT_REQUEST).exists());
        clear_quit_requests(install);
        assert!(!install.join("FridayNightFights").join("Saved").join(QUIT_REQUEST).exists());
    }

    #[test]
    fn status_file_is_read_from_the_project_saved_folder() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path();
        let saved = install.join("FridayNightFights").join("Saved");
        fs::create_dir_all(&saved).unwrap();
        fs::write(saved.join("uxa-status.json"), r#"{"playerCount":3}"#).unwrap();
        assert_eq!(player_count(install), Some(3));
    }
}
