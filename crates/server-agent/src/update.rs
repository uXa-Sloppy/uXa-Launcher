//! Stage server builds on a timer and apply them only in the daily window.
//! The agent may replace its own executable at any check. That path does not
//! stop the dedicated server.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use launcher_core::{
    download_url, download_with_resume, fetch_latest_release, is_newer, parse_sha256sums,
    replace_executable, restore_previous_launcher, running_from_cargo_target, verify_exe_file,
    DownloadRequest, ReleaseInfo, CHECKSUM_ASSET,
};
use serde::{Deserialize, Serialize};

use crate::runtime::{Agent, StopStyle};
use crate::schedule::{
    decide_tick, interval_from_hours, local_date, parse_clock, parse_zone, Tick,
};

const STATE_FILE: &str = "auto-update-state.json";
const STAGE_DIR: &str = "staged-server";
const STAGE_MARKER: &str = "staged-server.json";
const FAILED_TAG_FILE: &str = "failed-agent-tag";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct UpdateState {
    pub last_check_at: String,
    pub last_check_result: String,
    pub last_update_at: String,
    pub last_update_result: String,
    pub staged_version: String,
    pub last_maintenance_date: String,
    pub failed_agent_tag: String,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            last_check_at: String::new(),
            last_check_result: String::new(),
            last_update_at: String::new(),
            last_update_result: String::new(),
            staged_version: String::new(),
            last_maintenance_date: String::new(),
            failed_agent_tag: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfUpdateOutcome {
    Skipped,
    Restarting,
}

pub(crate) fn load_state(work: &Path) -> UpdateState {
    let path = work.join(STATE_FILE);
    let mut state = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<UpdateState>(&text).ok())
        .unwrap_or_default();
    let failed = work.join(FAILED_TAG_FILE);
    if let Ok(tag) = fs::read_to_string(&failed) {
        let tag = tag.trim();
        if !tag.is_empty() {
            state.failed_agent_tag = tag.to_string();
            state.last_update_at = stamp();
            state.last_update_result =
                format!("agent update {tag} failed and the previous build was restored");
        }
        let _ = fs::remove_file(&failed);
        save_state(work, &state);
    }
    if state.staged_version.is_empty() {
        if let Some(tag) = read_stage_tag(work) {
            state.staged_version = tag;
        }
    }
    state
}

fn save_state(work: &Path, state: &UpdateState) {
    let _ = fs::create_dir_all(work);
    let path = work.join(STATE_FILE);
    let tmp = work.join(format!("{STATE_FILE}.tmp"));
    let Ok(text) = serde_json::to_string_pretty(state) else {
        return;
    };
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn read_stage_tag(work: &Path) -> Option<String> {
    let text = fs::read_to_string(work.join(STAGE_MARKER)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tag = value.get("tag")?.as_str()?.trim();
    if tag.is_empty() {
        None
    } else {
        Some(tag.to_string())
    }
}

fn stamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn parse_stamp(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_str(text.trim(), "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

impl Agent {
    pub(crate) fn note_update(&self, message: impl AsRef<str>) {
        let message = message.as_ref();
        self.log(message.to_string());
        let _ = fs::create_dir_all(&self.config.log_dir);
        let path = self.config.log_dir.join("updates.log");
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let line = format!("{} {message}\n", stamp());
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn mutate_state(&self, change: impl FnOnce(&mut UpdateState)) {
        let Ok(mut state) = self.updates.lock() else {
            return;
        };
        change(&mut state);
        save_state(&self.config.work_dir, &state);
    }

    fn staged_version(&self) -> String {
        self.updates
            .lock()
            .map(|state| state.staged_version.clone())
            .unwrap_or_default()
    }

    pub(crate) fn clear_stage(&self) {
        let _ = fs::remove_dir_all(self.config.work_dir.join(STAGE_DIR));
        let _ = fs::remove_file(self.config.work_dir.join(STAGE_MARKER));
        self.mutate_state(|state| state.staged_version.clear());
    }

    pub(crate) fn discard_stage_if_not_newer(&self) {
        let staged = self.staged_version();
        if staged.is_empty() {
            return;
        }
        let installed = read_installed(&self.config.install_dir);
        if is_newer(&staged, Some(&installed)).unwrap_or(false) {
            return;
        }
        self.clear_stage();
        self.log("discarded staged build that is not newer than the installed server");
    }

    pub fn spawn_scheduler(self: &Arc<Self>) {
        if !self.config.auto_update.enabled {
            self.log("automatic updates are disabled");
            return;
        }
        if self.scheduler_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let agent = Arc::clone(self);
        tokio::spawn(async move {
            scheduler_loop(agent).await;
        });
    }

    /// Download a newer dedicated-server build into the work directory.
    /// Does not stop the running server and does not touch its Saved tree.
    pub async fn check_for_updates(&self) -> Result<launcher_core::AgentStatus, String> {
        let _guard = self.begin_real().map_err(|err| err.message)?;
        self.set_phase("checking");
        let outcome = self.check_server().await;
        self.set_phase("idle");
        let message = match &outcome {
            Ok(text) => text.clone(),
            Err(err) => format!("check failed: {err}"),
        };
        self.note_update(&message);
        self.mutate_state(|state| {
            state.last_check_at = stamp();
            state.last_check_result = message;
        });
        outcome?;
        Ok(self.status().await)
    }

    /// Install a staged server build, or do the daily restart when nothing newer is staged.
    /// Callers decide whether the clock is inside the maintenance window.
    pub async fn apply_maintenance(&self) -> Result<launcher_core::AgentStatus, String> {
        let _guard = self.begin_real().map_err(|err| err.message)?;
        let result = self.apply_maintenance_inner().await;
        self.set_phase("idle");
        match result {
            Ok(message) => {
                self.note_update(&message);
                self.mutate_state(|state| {
                    state.last_update_at = stamp();
                    state.last_update_result = message;
                });
                Ok(self.status().await)
            }
            Err(err) => {
                self.note_update(format!("maintenance failed: {err}"));
                self.mutate_state(|state| {
                    state.last_update_at = stamp();
                    state.last_update_result = format!("update failed: {err}");
                });
                Err(err)
            }
        }
    }

    /// Replace this executable when a newer agent release is published.
    /// Cargo target builds are skipped so tests do not overwrite themselves.
    /// A cargo-target skip happens before any download.
    pub async fn run_agent_self_update(&self) -> Result<SelfUpdateOutcome, String> {
        if !self.config.auto_update.agent_enabled {
            return Ok(SelfUpdateOutcome::Skipped);
        }
        let exe = std::env::current_exe().map_err(|err| err.to_string())?;
        if running_from_cargo_target(&exe) {
            self.note_update(format!(
                "skipping agent self-update; {} is a cargo target build",
                exe.display()
            ));
            return Ok(SelfUpdateOutcome::Skipped);
        }
        let Some(release) = self.fetch_agent_release().await? else {
            self.note_update("no agent release is published");
            return Ok(SelfUpdateOutcome::Skipped);
        };
        if release.draft || release.prerelease {
            self.note_update(format!("ignoring pre-release agent {}", release.tag));
            return Ok(SelfUpdateOutcome::Skipped);
        }
        let current = env!("CARGO_PKG_VERSION");
        if !is_newer(&release.tag, Some(current)).map_err(|err| err.to_string())? {
            self.note_update(format!("agent {current} is current"));
            return Ok(SelfUpdateOutcome::Skipped);
        }
        let failed = self
            .updates
            .lock()
            .map(|state| state.failed_agent_tag.clone())
            .unwrap_or_default();
        if !failed.is_empty() && !is_newer(&release.tag, Some(&failed)).unwrap_or(false) {
            self.note_update(format!(
                "skipping agent {failed}; that update already failed"
            ));
            return Ok(SelfUpdateOutcome::Skipped);
        }
        let staged = self.download_agent_exe(&release).await?;
        replace_executable(&exe, &staged).map_err(|err| err.to_string())?;
        let failed_file = self.config.work_dir.join(FAILED_TAG_FILE);
        let parent = std::process::id();
        if let Err(err) = spawn_helper(
            &self.config.work_dir,
            parent,
            &exe,
            &failed_file,
            &release.tag,
        ) {
            let _ = restore_previous_launcher(&exe);
            return Err(format!(
                "could not start the agent updater ({err}). The previous executable was restored."
            ));
        }
        self.mutate_state(|state| {
            state.last_update_at = stamp();
            state.last_update_result = format!("agent replaced itself with {}", release.tag);
            if state.failed_agent_tag == release.tag {
                state.failed_agent_tag.clear();
            }
        });
        self.note_update(format!(
            "agent executable replaced with {}; restarting the agent without stopping the game server",
            release.tag
        ));
        Ok(SelfUpdateOutcome::Restarting)
    }

    async fn check_server(&self) -> Result<String, String> {
        if !self.config.auto_update.server_enabled {
            return Ok("server update checks are disabled".into());
        }
        let source = self.release_source();
        let releases = launcher_core::list_matching_releases(&self.client, &source)
            .await
            .map_err(|err| err.to_string())?;
        let Some(newest) = select_newest(releases) else {
            return Ok("no matching server release".into());
        };
        let installed = read_installed(&self.config.install_dir);
        if !is_newer(&newest.tag, Some(&installed)).unwrap_or(false) {
            self.discard_stage_if_not_newer();
            let label = if installed.is_empty() {
                "nothing installed".to_string()
            } else {
                installed
            };
            return Ok(format!("up to date at {label}"));
        }
        let staged = self.staged_version();
        let stage_dir = self.config.work_dir.join(STAGE_DIR);
        if staged == newest.tag && stage_dir.is_dir() {
            return Ok(format!("staged {} is already current", newest.tag));
        }
        self.download_release(&newest, &stage_dir).await?;
        let marker = serde_json::json!({
            "tag": newest.tag,
            "asset": newest.asset.name,
            "stagedAt": stamp(),
        });
        fs::write(
            self.config.work_dir.join(STAGE_MARKER),
            serde_json::to_string_pretty(&marker).map_err(|err| err.to_string())?,
        )
        .map_err(|err| err.to_string())?;
        let tag = newest.tag.clone();
        self.mutate_state(|state| state.staged_version = tag.clone());
        Ok(format!("staged {tag}"))
    }

    async fn apply_maintenance_inner(&self) -> Result<String, String> {
        let mut staged = self.staged_version();
        let installed = read_installed(&self.config.install_dir);
        if !staged.is_empty() && !is_newer(&staged, Some(&installed)).unwrap_or(false) {
            self.clear_stage();
            staged.clear();
        }
        if !staged.is_empty() {
            let staging = self.config.work_dir.join(STAGE_DIR);
            if !staging.is_dir() {
                self.clear_stage();
                return Err(format!("staged build {staged} is missing from disk"));
            }
            let tag = staged.clone();
            let installed_now = self
                .install_prepared(&staging, &tag, StopStyle::Grace, true)
                .await
                .map_err(|err| err.message);
            if !staging.exists() {
                self.clear_stage();
            }
            installed_now?;
            return Ok(format!("installed {tag}"));
        }
        if !self.config.auto_update.daily_restart {
            return Ok("maintenance window had nothing to apply".into());
        }
        if !self.config.install_dir.exists() {
            return Ok("daily restart skipped; no server is installed".into());
        }
        self.restart_in_place().await?;
        Ok("restarted".into())
    }

    async fn fetch_agent_release(&self) -> Result<Option<launcher_core::FullRelease>, String> {
        let owner = &self.config.auto_update.agent_owner;
        let repo = &self.config.auto_update.agent_repo;
        let anon = fetch_latest_release(&self.client, &self.config.api_base, owner, repo, None)
            .await
            .map_err(|err| err.to_string())?;
        if anon.is_some() || self.config.github_token.is_none() {
            return Ok(anon);
        }
        fetch_latest_release(
            &self.client,
            &self.config.api_base,
            owner,
            repo,
            self.config.github_token.as_deref(),
        )
        .await
        .map_err(|err| err.to_string())
    }

    async fn download_agent_exe(
        &self,
        release: &launcher_core::FullRelease,
    ) -> Result<PathBuf, String> {
        let asset_name = &self.config.auto_update.agent_asset;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name.eq_ignore_ascii_case(asset_name))
            .ok_or_else(|| {
                format!(
                    "release {} has no {asset_name}. The agent was not changed.",
                    release.tag
                )
            })?;
        let sums = release
            .assets
            .iter()
            .find(|asset| asset.name.eq_ignore_ascii_case(CHECKSUM_ASSET));
        let downloads = self.config.work_dir.join("downloads");
        fs::create_dir_all(&downloads).map_err(|err| err.to_string())?;
        let exe_path = downloads.join(asset_name);
        // Never resume from a file left by an older update: same-size leftovers would be
        // treated as complete and fail (or worse, pass) verification.
        let _ = fs::remove_file(&exe_path);
        let _ = fs::remove_file(downloads.join(CHECKSUM_ASSET));
        let url = download_url(asset)
            .map_err(|err| err.to_string())?
            .to_string();
        download_with_resume(
            DownloadRequest {
                client: &self.client,
                url: &url,
                dest: &exe_path,
                expected_size: asset.size,
                token: self.config.github_token.as_deref(),
            },
            |_| {},
        )
        .await
        .map_err(|err| err.to_string())?;
        let sha = if let Some(sums) = sums {
            let sums_path = downloads.join(CHECKSUM_ASSET);
            let sums_url = download_url(sums)
                .map_err(|err| err.to_string())?
                .to_string();
            download_with_resume(
                DownloadRequest {
                    client: &self.client,
                    url: &sums_url,
                    dest: &sums_path,
                    expected_size: sums.size,
                    token: self.config.github_token.as_deref(),
                },
                |_| {},
            )
            .await
            .map_err(|err| err.to_string())?;
            let text = fs::read_to_string(&sums_path).map_err(|err| err.to_string())?;
            Some(parse_sha256sums(&text, asset_name).map_err(|err| err.to_string())?)
        } else {
            asset.digest.clone()
        };
        require_verified_exe(&exe_path, asset.size, sha.as_deref())?;
        Ok(exe_path)
    }

    fn decide(&self, booted: DateTime<Utc>, startup_done: bool) -> Option<Tick> {
        let auto = &self.config.auto_update;
        let restart = parse_clock(&auto.restart_time)?;
        let tz = parse_zone(&auto.restart_time_zone).ok()?;
        let mut check_times = Vec::new();
        for text in &auto.check_times {
            check_times.push(parse_clock(text)?);
        }
        let (last_check_at, last_maintenance) = {
            let state = self.updates.lock().ok()?;
            (
                state.last_check_at.clone(),
                state.last_maintenance_date.clone(),
            )
        };
        Some(decide_tick(
            Utc::now(),
            booted,
            chrono::Duration::seconds(auto.check_after_start_secs as i64),
            interval_from_hours(auto.interval_hours),
            parse_stamp(&last_check_at),
            startup_done,
            &check_times,
            restart,
            tz,
            chrono::Duration::minutes(auto.restart_window_minutes as i64),
            NaiveDate::parse_from_str(&last_maintenance, "%Y-%m-%d").ok(),
        ))
    }

    fn mark_maintenance_today(&self) {
        let Ok(tz) = parse_zone(&self.config.auto_update.restart_time_zone) else {
            return;
        };
        let date = local_date(Utc::now(), tz).format("%Y-%m-%d").to_string();
        self.mutate_state(|state| state.last_maintenance_date = date);
    }

    async fn finish_self_update(&self) {
        if !self.config.auto_update.agent_enabled {
            return;
        }
        match self.run_agent_self_update().await {
            Ok(SelfUpdateOutcome::Restarting) => std::process::exit(0),
            Ok(SelfUpdateOutcome::Skipped) => {}
            Err(err) => self.note_update(format!("agent self-update failed: {err}")),
        }
    }
}

async fn scheduler_loop(agent: Arc<Agent>) {
    let auto = &agent.config.auto_update;
    agent.note_update(format!(
        "automatic updates on; check every {}h; game server restarts only at {} {}",
        auto.interval_hours, auto.restart_time, auto.restart_time_zone
    ));
    let booted = Utc::now();
    let mut startup_done = false;
    loop {
        match agent.decide(booted, startup_done) {
            Some(Tick::Maintenance) => {
                agent.mark_maintenance_today();
                let _ = agent.check_for_updates().await;
                startup_done = true;
                let _ = agent.apply_maintenance().await;
                agent.finish_self_update().await;
            }
            Some(Tick::Check) => {
                let _ = agent.check_for_updates().await;
                startup_done = true;
                agent.finish_self_update().await;
            }
            Some(Tick::Sleep) | None => {}
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

fn read_installed(install: &Path) -> String {
    fs::read_to_string(install.join(".uxa-server-version"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn select_newest(releases: Vec<ReleaseInfo>) -> Option<ReleaseInfo> {
    let best = newest_tag(releases.iter().map(|release| release.tag.as_str()))?.to_string();
    releases.into_iter().find(|release| release.tag == best)
}

pub(crate) fn newest_tag<'a>(tags: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let mut best: Option<&str> = None;
    for tag in tags {
        let take = match best {
            None => true,
            Some(current) => is_newer(tag, Some(current)).unwrap_or(false),
        };
        if take {
            best = Some(tag);
        }
    }
    best
}

fn require_verified_exe(path: &Path, size: u64, sha: Option<&str>) -> Result<(), String> {
    let sha = sha.map(str::trim).filter(|value| !value.is_empty());
    let Some(sha) = sha else {
        return Err(
            "release has no SHA256SUMS.txt entry or asset digest; the agent was not changed".into(),
        );
    };
    verify_exe_file(path, size, Some(sha)).map_err(|err| err.to_string())
}

fn spawn_helper(
    work: &Path,
    parent: u32,
    exe: &Path,
    failed_file: &Path,
    tag: &str,
) -> Result<Child, String> {
    fs::create_dir_all(work).map_err(|err| err.to_string())?;
    let mode = if std::env::args().any(|arg| arg == "--service") {
        "service"
    } else {
        "foreground"
    };
    #[cfg(windows)]
    let (program, script) = ("powershell", work.join("agent-updater.ps1"));
    #[cfg(not(windows))]
    let (program, script) = ("sh", work.join("agent-updater.sh"));
    fs::write(&script, helper_script()).map_err(|err| err.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&script) {
            let mut perm = meta.permissions();
            perm.set_mode(perm.mode() | 0o755);
            let _ = fs::set_permissions(&script, perm);
        }
    }
    let old = launcher_core::backup_path(exe);
    let mut command = Command::new(program);
    let script_arg = script.display().to_string();
    #[cfg(windows)]
    {
        command.args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
            script_arg.as_str(),
        ]);
    }
    #[cfg(not(windows))]
    {
        command.arg(&script_arg);
    }
    command
        .arg(parent.to_string())
        .arg(exe)
        .arg(&old)
        .arg(failed_file)
        .arg(tag)
        .arg(mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000200);
    }
    command.spawn().map_err(|err| err.to_string())
}

fn helper_script() -> &'static str {
    #[cfg(windows)]
    {
        return r#"param($parent, $exe, $old, $failed, $tag, $mode)
while (Get-Process -Id $parent -ErrorAction SilentlyContinue) { Start-Sleep -Milliseconds 200 }
function Start-Agent {
  if ($mode -eq "service") {
    & sc.exe start uXaServerAgent | Out-Null
    return $LASTEXITCODE
  }
  $proc = Start-Process -FilePath $exe -PassThru -WindowStyle Hidden
  Start-Sleep -Seconds 1
  if (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue) { return 0 }
  return 1
}
if ((Start-Agent) -eq 0) { exit 0 }
if (Test-Path -LiteralPath $old) {
  Remove-Item -LiteralPath $exe -Force -ErrorAction SilentlyContinue
  Move-Item -LiteralPath $old -Destination $exe -Force
}
Set-Content -LiteralPath $failed -Value $tag -NoNewline
Start-Agent | Out-Null
exit 0
"#;
    }
    #[cfg(not(windows))]
    {
        r#"#!/bin/sh
parent="$1"
exe="$2"
old="$3"
failed="$4"
tag="$5"
mode="$6"
while kill -0 "$parent" 2>/dev/null; do
  sleep 0.2
done
start_one() {
  if [ "$mode" = "service" ]; then
    sc start uXaServerAgent >/dev/null 2>&1
    return $?
  fi
  nohup "$exe" >/dev/null 2>&1 &
  child=$!
  sleep 1
  if kill -0 "$child" 2>/dev/null; then
    return 0
  fi
  return 1
}
if start_one; then
  exit 0
fi
if [ -f "$old" ]; then
  rm -f "$exe"
  mv "$old" "$exe"
fi
printf '%s\n' "$tag" > "$failed"
start_one
exit 0
"#
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use launcher_core::file_sha256;

    #[test]
    fn newest_tag_ignores_list_order_and_suffixes() {
        let tags = ["v0.9.6", "v0.9.6.1", "v0.9.7.2-build18g", "v0.9.7.1"];
        assert_eq!(newest_tag(tags).unwrap(), "v0.9.7.2-build18g");
        assert_eq!(
            newest_tag(["v0.9.7.2-build18g", "v0.9.6", "v0.9.7.2"]).unwrap(),
            "v0.9.7.2"
        );
        assert!(!is_newer("v0.9.6.1", Some("v0.9.7.2-build18g")).unwrap());
    }

    #[test]
    fn agent_exe_requires_mz_and_a_checksum() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("uxa-server-agent.exe");
        let bytes = b"MZ-agent-bytes";
        fs::write(&path, bytes).unwrap();
        let sha = file_sha256(&path).unwrap();
        let err = require_verified_exe(&path, bytes.len() as u64, None).unwrap_err();
        assert!(err.contains("SHA256SUMS"), "{err}");
        let err = require_verified_exe(&path, bytes.len() as u64, Some("abcd")).unwrap_err();
        assert!(
            err.contains("checksum") || err.contains("not a Windows"),
            "{err}"
        );
        require_verified_exe(&path, bytes.len() as u64, Some(&sha)).unwrap();
        fs::write(&path, b"NO").unwrap();
        let err = require_verified_exe(&path, 2, Some(&file_sha256(&path).unwrap())).unwrap_err();
        assert!(err.contains("not a Windows"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn helper_restores_previous_exe_when_replacement_exits() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("uxa-server-agent.exe");
        let marker = temp.path().join("marker");
        let pidfile = temp.path().join("restored.pid");
        let marker_text = marker.display().to_string();
        let pid_text = pidfile.display().to_string();
        let old_body =
            format!("#!/bin/sh\necho $$ > '{pid_text}'\necho old >> '{marker_text}'\nsleep 30\n");
        fs::write(&exe, &old_body).unwrap();
        let newer = temp.path().join("newer");
        fs::write(
            &newer,
            format!("#!/bin/sh\necho new >> '{marker_text}'\nexit 1\n"),
        )
        .unwrap();
        for path in [&exe, &newer] {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = fs::metadata(path).unwrap().permissions();
            perm.set_mode(0o755);
            fs::set_permissions(path, perm).unwrap();
        }
        replace_executable(&exe, &newer).unwrap();
        // The sleeper is reparented to init so it is reaped. A direct child would
        // stay a zombie, and `kill -0` treats a zombie as alive.
        let listed = Command::new("sh")
            .arg("-c")
            .arg("sleep 0.4 >/dev/null 2>&1 & echo $!")
            .output()
            .unwrap();
        let parent: u32 = String::from_utf8(listed.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let failed = temp.path().join(FAILED_TAG_FILE);
        let mut helper = spawn_helper(temp.path(), parent, &exe, &failed, "v9.9.9").unwrap();
        let status = helper.wait().unwrap();
        assert!(status.success(), "helper status {status}");
        assert_eq!(fs::read_to_string(&failed).unwrap().trim(), "v9.9.9");
        let body = fs::read_to_string(&exe).unwrap();
        assert!(body.contains("echo old"), "{body}");
        let events = fs::read_to_string(&marker).unwrap();
        assert!(events.contains("new"), "{events}");
        assert!(events.contains("old"), "{events}");
        let restored = fs::read_to_string(&pidfile).unwrap();
        let restored_pid: u32 = restored.trim().parse().unwrap();
        assert!(Path::new(&format!("/proc/{restored_pid}")).exists());
        let _ = Command::new("kill")
            .args(["-KILL", &restored_pid.to_string()])
            .status();
    }
}
