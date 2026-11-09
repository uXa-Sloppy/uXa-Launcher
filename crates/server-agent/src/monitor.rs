//! Read-only monitoring for the launcher's Server Status view: GET /v1/monitor.
//!
//! Players come from `<game Saved>/uxa-status.json`, which the game server writes:
//!   {"playerCount":2,"players":[{"account":"Dave","character":"Sloppy"}],
//!    "map":"Purgatory","updatedUtc":"2026-11-09T20:00:00Z"}
//! When that file is missing or stale, the newest game log is scanned for
//! "Join succeeded: <name>" lines (players that joined since the log started; not live).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use serde::Serialize;
use sysinfo::{Disks, Pid, ProcessesToUpdate, System};

use crate::runtime::Agent;

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlayerInfo {
    pub account: String,
    pub character: String,
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlayerStatus {
    pub count: Option<u32>,
    pub players: Vec<PlayerInfo>,
    /// "status-file", "status-file-stale", "log-joins" or "none".
    pub source: String,
    pub updated_utc: String,
    pub map: String,
}

fn saved_dirs(install: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = launcher_core::resolve_preserved(install, Path::new("Saved"))
        .into_iter()
        .map(|rel| install.join(rel))
        .collect();
    let root = install.join("Saved");
    if !dirs.contains(&root) && root.is_dir() {
        dirs.push(root);
    }
    dirs
}

pub(crate) fn read_player_status(install: &Path) -> Option<PlayerStatus> {
    for dir in saved_dirs(install) {
        let path = dir.join("uxa-status.json");
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let players: Vec<PlayerInfo> = value
            .get("players")
            .and_then(|p| p.as_array())
            .map(|list| {
                list.iter()
                    .map(|p| PlayerInfo {
                        account: p.get("account").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                        character: p.get("character").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let count = value
            .get("playerCount")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .or(if players.is_empty() { None } else { Some(players.len() as u32) });
        let stale = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age.as_secs() > 120)
            .unwrap_or(false);
        return Some(PlayerStatus {
            count,
            players,
            source: if stale { "status-file-stale".into() } else { "status-file".into() },
            updated_utc: value.get("updatedUtc").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            map: value.get("map").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        });
    }
    None
}

fn newest_log(install: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in saved_dirs(install) {
        let Ok(read) = fs::read_dir(dir.join("Logs")) else { continue };
        for entry in read.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("log") {
                continue;
            }
            let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                best = Some((modified, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

pub(crate) fn players_from_log(text: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(idx) = line.find("Join succeeded: ") {
            let name = line[idx + 16..].trim().to_string();
            if !name.is_empty() && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn dir_size(path: &Path) -> u64 {
    let Ok(read) = fs::read_dir(path) else { return 0 };
    read.flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() { dir_size(&p) } else { e.metadata().map(|m| m.len()).unwrap_or(0) }
        })
        .sum()
}

impl Agent {
    pub async fn monitor(&self) -> serde_json::Value {
        let status = self.status().await;
        let install = self.config.install_dir.clone();

        let log_path = newest_log(&install);
        let log_text = log_path
            .as_ref()
            .and_then(|p| fs::read(p).ok())
            .map(|b| String::from_utf8_lossy(&b[b.len().saturating_sub(4 * 1024 * 1024)..]).into_owned())
            .unwrap_or_default();
        let players = match read_player_status(&install) {
            Some(p) => p,
            None if !log_text.is_empty() && status.online => {
                let names = players_from_log(&log_text);
                PlayerStatus {
                    count: None,
                    players: names.into_iter().map(|n| PlayerInfo { account: n, character: String::new() }).collect(),
                    source: "log-joins".into(),
                    ..Default::default()
                }
            }
            None => PlayerStatus { source: "none".into(), ..Default::default() },
        };

        let pid = self.server_pid().await;
        let system = tokio::task::spawn_blocking({
            let install = install.clone();
            move || {
                let mut sys = System::new();
                sys.refresh_memory();
                sys.refresh_cpu_usage();
                std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
                sys.refresh_cpu_usage();
                let mut proc_mem = None;
                let mut proc_cpu = None;
                if let Some(pid) = pid {
                    let pid = Pid::from_u32(pid);
                    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
                    if let Some(p) = sys.process(pid) {
                        proc_mem = Some(p.memory() / (1024 * 1024));
                        proc_cpu = Some(p.cpu_usage());
                    }
                }
                let disks = Disks::new_with_refreshed_list();
                let canonical = install.canonicalize().unwrap_or(install.clone());
                let disk = disks
                    .list()
                    .iter()
                    .filter(|d| canonical.starts_with(d.mount_point()))
                    .max_by_key(|d| d.mount_point().as_os_str().len());
                serde_json::json!({
                    "cpuPercent": (sys.global_cpu_usage() * 10.0).round() / 10.0,
                    "cpuCount": sys.cpus().len(),
                    "memTotalMb": sys.total_memory() / (1024 * 1024),
                    "memUsedMb": sys.used_memory() / (1024 * 1024),
                    "diskMount": disk.map(|d| d.mount_point().display().to_string()).unwrap_or_default(),
                    "diskTotalGb": disk.map(|d| d.total_space() as f64 / 1e9).map(|v| (v * 10.0).round() / 10.0),
                    "diskFreeGb": disk.map(|d| d.available_space() as f64 / 1e9).map(|v| (v * 10.0).round() / 10.0),
                    "serverMemMb": proc_mem,
                    "serverCpuPercent": proc_cpu,
                    "uptimeHostSecs": System::uptime(),
                })
            }
        })
        .await
        .unwrap_or_else(|_| serde_json::json!({}));

        let mut backups: Vec<serde_json::Value> = Vec::new();
        if let Ok(read) = fs::read_dir(&self.config.backup_dir) {
            let mut list: Vec<_> = read.flatten().filter(|e| e.path().is_dir()).collect();
            list.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
            for entry in list.into_iter().take(20) {
                backups.push(serde_json::json!({
                    "name": entry.file_name().to_string_lossy(),
                    "sizeMb": dir_size(&entry.path()) / (1024 * 1024),
                }));
            }
        }
        let server_log = self.server_log();
        let crashes = server_log.get("crashes").cloned().unwrap_or(serde_json::json!([]));
        let events = self.recent_events(50);
        let restarts_24h = events
            .iter()
            .filter(|e| matches!(e.get("kind").and_then(|k| k.as_str()), Some("start" | "crash-restart")))
            .filter(|e| {
                e.get("at")
                    .and_then(|a| a.as_str())
                    .and_then(|a| chrono::DateTime::parse_from_rfc3339(a).ok())
                    .map(|t| chrono::Utc::now().signed_duration_since(t).num_hours() < 24)
                    .unwrap_or(false)
            })
            .count();
        let accounts = self.accounts_status().await;
        serde_json::json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "status": status,
            "agentUptimeSecs": self.booted_at.elapsed().as_secs(),
            "watchdog": self.expect_running.load(Ordering::Acquire),
            "players": players,
            "system": system,
            "events": events,
            "restarts24h": restarts_24h,
            "crashes": crashes,
            "logFile": log_path.map(|p| p.display().to_string()).unwrap_or_default(),
            "logTail": tail(&log_text, 80),
            "accounts": accounts,
            "backups": backups,
            "backupDir": self.config.backup_dir.display().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_lines_give_unique_names() {
        let log = "[2026.11.09-20.00.00:000][  0]LogNet: Join succeeded: Dave\nnoise\nLogNet: Join succeeded: Dolmar\nLogNet: Join succeeded: Dave\n";
        assert_eq!(players_from_log(&log), vec!["Dave", "Dolmar"]);
    }

    #[test]
    fn status_file_in_project_saved() {
        let dir = tempfile::tempdir().unwrap();
        let saved = dir.path().join("FridayNightFights/Saved");
        fs::create_dir_all(&saved).unwrap();
        fs::write(saved.join("uxa-status.json"), r#"{"playerCount":1,"players":[{"account":"Dave","character":"Sloppy"}],"map":"Purgatory"}"#).unwrap();
        let p = read_player_status(dir.path()).unwrap();
        assert_eq!(p.count, Some(1));
        assert_eq!(p.players[0].character, "Sloppy");
        assert_eq!(p.source, "status-file");
    }
}
