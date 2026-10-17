//! Scheduled updates stage a build without stopping the server, then install it
//! only when apply_maintenance is called. Saved from the live tree wins over a
//! Saved folder shipped in the zip, including after a failed build is rolled back.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use uxa_server_agent::{Agent, AgentConfig, AutoUpdateConfig, SelfUpdateOutcome};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

struct Remote {
    files: HashMap<String, Vec<u8>>,
    releases: String,
}

fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default().unix_permissions(0o755);
        for (name, bytes) in files {
            writer.start_file(*name, opts).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

fn good_script(log: &str) -> String {
    format!(
        "#!/bin/sh\n\
         printf 'start args %s\\n' \"$*\" >> '{log}'\n\
         while IFS= read -r line; do\n\
           printf '%s\\n' \"$line\" >> '{log}'\n\
           case \"$line\" in\n\
             quit*) exit 0 ;;\n\
           esac\n\
         done\n"
    )
}

fn player_script(log: &str) -> String {
    format!(
        "#!/bin/sh\n\
         printf 'start args %s\\n' \"$*\" >> '{log}'\n\
         dir=$(CDPATH= cd -- \"$(dirname \"$0\")\" && pwd)\n\
         mkdir -p \"$dir/Saved\"\n\
         printf '%s\\n' '{{\"playerCount\":1}}' > \"$dir/Saved/uxa-status.json\"\n\
         while IFS= read -r line; do\n\
           printf '%s\\n' \"$line\" >> '{log}'\n\
           case \"$line\" in\n\
             say*) printf '%s\\n' '{{\"playerCount\":0}}' > \"$dir/Saved/uxa-status.json\" ;;\n\
             quit*) exit 0 ;;\n\
           esac\n\
         done\n"
    )
}

fn bad_script(log: &str) -> String {
    format!(
        "#!/bin/sh\n\
         printf 'bad-start\\n' >> '{log}'\n\
         dir=$(CDPATH= cd -- \"$(dirname \"$0\")\" && pwd)\n\
         mkdir -p \"$dir/FridayNightFights/Saved\"\n\
         printf '%s' 'CORRUPT' > \"$dir/FridayNightFights/Saved/world.sav\"\n\
         printf '%s' 'CORRUPT' > \"$dir/FridayNightFights/Saved/account.txt\"\n\
         exit 1\n"
    )
}

fn start_remote(state: Arc<Mutex<Remote>>) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url().to_string();
            let remote = state.lock().unwrap();
            let (status, body, content_type) = if url.contains("/releases/latest") {
                (404, b"none".to_vec(), "application/json")
            } else if url.contains("/releases") {
                (
                    200,
                    remote.releases.clone().into_bytes(),
                    "application/json",
                )
            } else if let Some(name) = url.rsplit('/').next() {
                if let Some(bytes) = remote.files.get(name) {
                    (200, bytes.clone(), "application/octet-stream")
                } else {
                    (404, b"missing".to_vec(), "text/plain")
                }
            } else {
                (404, b"missing".to_vec(), "text/plain")
            };
            drop(remote);
            let response = tiny_http::Response::from_data(body)
                .with_status_code(status)
                .with_header(
                    tiny_http::Header::from_bytes(b"Content-Type", content_type.as_bytes())
                        .unwrap(),
                );
            let _ = request.respond(response);
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn release_json(api: &str, entries: &[(&str, &str, bool, &str)]) -> String {
    let mut body = String::from("[");
    for (index, (tag, name, pre, file)) in entries.iter().enumerate() {
        if index > 0 {
            body.push(',');
        }
        let size = 0; // replaced below by the caller when it knows the bytes
        let _ = size;
        body.push_str(&format!(
            r#"{{"tag_name":"{tag}","name":"{name}","draft":false,"prerelease":{pre},"assets":[{{"name":"FridayNightFights-Server-Win64.zip","browser_download_url":"{api}/{file}","size":SIZE_{file}}}]}}"#
        ));
    }
    body.push(']');
    body
}

fn with_sizes(template: &str, files: &HashMap<String, Vec<u8>>) -> String {
    let mut text = template.to_string();
    for (name, bytes) in files {
        text = text.replace(&format!("SIZE_{name}"), &bytes.len().to_string());
    }
    text
}

fn test_config(root: &Path, api: &str) -> AgentConfig {
    let mut auto = AutoUpdateConfig::disabled();
    auto.grace_seconds = 30;
    auto.daily_restart = true;
    auto.server_enabled = true;
    auto.agent_enabled = true;
    auto.keep_backups = 7;
    let mut config = AgentConfig {
        install_dir: root.join("FridayNightFightsServer"),
        executable: "FridayNightFightsServer.exe".into(),
        port: 7777,
        args: Vec::new(),
        preserve: vec!["Saved".into(), "Config".into()],
        owner: "uXa-Sloppy".into(),
        repo: "FridayNightFights".into(),
        asset_contains: vec!["Server".into(), "Win64".into()],
        asset_pattern: String::new(),
        include_prereleases: true,
        api_base: api.into(),
        github_token: None,
        listen: "127.0.0.1:9".into(),
        warn_seconds: 0,
        health_wait_secs: 1,
        quit_wait_secs: 3,
        require_tcp: false,
        tls_cert: Path::new("").into(),
        tls_key: Path::new("").into(),
        backup_dir: root.join("backups"),
        work_dir: root.join("work"),
        log_dir: root.join("logs"),
        secret: "test-secret-not-for-production".into(),
        auto_update: auto,
    };
    config.finish_paths().unwrap();
    config
}

fn read_pid(work: &Path) -> u32 {
    let text = fs::read_to_string(work.join("server-process.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value.get("pid").and_then(|v| v.as_u64()).unwrap() as u32
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn kill_pid(pid: u32) {
    if pid != 0 {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn update_does_not_delete_or_overwrite_saved() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("events.log");
    let log_text = log.display().to_string();
    let old = zip_bytes(&[(
        "FridayNightFightsServer.exe",
        good_script(&log_text).as_bytes(),
    )]);
    let newer = zip_bytes(&[
        (
            "FridayNightFightsServer.exe",
            good_script(&log_text).as_bytes(),
        ),
        ("FridayNightFights/Saved/wiped.txt", b"WIPED"),
    ]);
    let broken = zip_bytes(&[(
        "FridayNightFightsServer.exe",
        bad_script(&log_text).as_bytes(),
    )]);
    let mut files = HashMap::new();
    files.insert("old.zip".into(), old);
    files.insert("newer.zip".into(), newer);
    files.insert("broken.zip".into(), broken);

    let state = Arc::new(Mutex::new(Remote {
        releases: String::new(),
        files,
    }));
    let api = start_remote(Arc::clone(&state));
    {
        let mut remote = state.lock().unwrap();
        let template = release_json(
            &api,
            &[
                ("v0.9.6", "Older", false, "old.zip"),
                ("v0.9.7.2-build18g", "Newer", true, "newer.zip"),
            ],
        );
        remote.releases = with_sizes(&template, &remote.files);
    }

    let agent = Agent::new(test_config(temp.path(), &api)).unwrap();
    agent
        .deploy("v0.9.6")
        .await
        .expect("install the older build");
    let install = temp.path().join("FridayNightFightsServer");
    fs::create_dir_all(install.join("FridayNightFights/Saved")).unwrap();
    let world = install.join("FridayNightFights/Saved/world.sav");
    let account = install.join("FridayNightFights/Saved/account.txt");
    fs::write(&world, b"WORLD").unwrap();
    fs::write(&account, b"ACCOUNT").unwrap();
    let pid = read_pid(&temp.path().join("work"));
    assert!(pid_alive(pid), "server should be running after deploy");

    let skipped = agent.run_agent_self_update().await.unwrap();
    assert_eq!(skipped, SelfUpdateOutcome::Skipped);
    assert!(pid_alive(pid), "self-update skip must not stop the server");
    let before = fs::read_to_string(&log).unwrap();
    assert!(
        !before.contains("quit"),
        "deploy of a fresh install should not quit\n{before}"
    );

    let staged = agent.check_for_updates().await.expect("stage");
    assert_eq!(staged.staged_version, "v0.9.7.2-build18g");
    assert!(staged.online);
    assert_eq!(fs::read(&world).unwrap(), b"WORLD");
    assert_eq!(fs::read(&account).unwrap(), b"ACCOUNT");
    assert!(!before_quit(&log), "staging must not stop the server");
    let updates = fs::read_to_string(temp.path().join("logs/updates.log")).unwrap();
    assert!(updates.contains("staged v0.9.7.2-build18g"), "{updates}");

    let applied = agent.apply_maintenance().await.expect("apply staged build");
    assert_eq!(applied.version, "v0.9.7.2-build18g");
    assert!(applied.online);
    assert!(
        applied.staged_version.is_empty(),
        "installed stage should be cleared"
    );
    assert_eq!(fs::read(&world).unwrap(), b"WORLD");
    assert_eq!(fs::read(&account).unwrap(), b"ACCOUNT");
    assert!(!install.join("FridayNightFights/Saved/wiped.txt").exists());
    let after_apply = fs::read_to_string(&log).unwrap();
    assert!(after_apply.contains("quit"), "{after_apply}");
    assert!(
        walk_contains(&temp.path().join("backups"), "world.sav", b"WORLD"),
        "timestamped backup should keep the world"
    );

    {
        let mut remote = state.lock().unwrap();
        let template = release_json(
            &api,
            &[
                ("v0.9.6", "Older", false, "old.zip"),
                ("v0.9.7.2-build18g", "Newer", true, "newer.zip"),
                ("v0.9.8", "Broken", false, "broken.zip"),
            ],
        );
        remote.releases = with_sizes(&template, &remote.files);
    }
    let staged_bad = agent.check_for_updates().await.expect("stage broken");
    assert_eq!(staged_bad.staged_version, "v0.9.8");
    assert_eq!(
        fs::read(&world).unwrap(),
        b"WORLD",
        "staging the bad build must not touch live saves"
    );

    let failed = agent
        .apply_maintenance()
        .await
        .expect_err("broken build rolls back");
    assert!(failed.contains("Previous build restored"), "{failed}");
    let status = agent.status().await;
    assert_eq!(status.version, "v0.9.7.2-build18g");
    assert!(status.online, "previous build should be running");
    assert_eq!(fs::read(&world).unwrap(), b"WORLD");
    assert_eq!(fs::read(&account).unwrap(), b"ACCOUNT");
    assert!(!walk_contains(&install, "world.sav", b"CORRUPT"));
    let events = fs::read_to_string(&log).unwrap();
    assert!(events.contains("bad-start"), "{events}");

    let pid = read_pid(&temp.path().join("work"));
    let bridge = bridge_pid(&temp.path().join("work"));
    drop(agent);
    assert!(
        pid_alive(pid),
        "dropping the agent must leave the dedicated server running"
    );
    let adopted = Agent::new(test_config(temp.path(), &api)).unwrap();
    let status = adopted.status().await;
    assert!(
        status.online,
        "a restarted agent should adopt the live server"
    );
    adopted
        .stop()
        .await
        .expect("quit through the surviving stdin bridge");
    assert!(!adopted.status().await.online);
    kill_pid(pid);
    kill_pid(bridge);
}

#[cfg(unix)]
#[tokio::test]
async fn maintenance_waits_for_players_then_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("events.log");
    let log_text = log.display().to_string();
    let good = zip_bytes(&[(
        "FridayNightFightsServer.exe",
        player_script(&log_text).as_bytes(),
    )]);
    let mut files = HashMap::new();
    files.insert("good.zip".into(), good);
    let state = Arc::new(Mutex::new(Remote {
        releases: String::new(),
        files,
    }));
    let api = start_remote(Arc::clone(&state));
    {
        let mut remote = state.lock().unwrap();
        let template = release_json(&api, &[("v1.0.0", "Only", false, "good.zip")]);
        remote.releases = with_sizes(&template, &remote.files);
    }
    let agent = Agent::new(test_config(temp.path(), &api)).unwrap();
    agent.deploy("v1.0.0").await.expect("start");
    let started = Instant::now();
    let status = agent.apply_maintenance().await.expect("daily restart");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "grace should end when players leave"
    );
    assert!(status.online);
    assert_eq!(status.version, "v1.0.0");
    let updates = fs::read_to_string(temp.path().join("logs/updates.log")).unwrap();
    assert!(
        updates.contains("players have left") || agent_log_contains(&agent, "players have left"),
        "{updates}"
    );
    let pid = read_pid(&temp.path().join("work"));
    let bridge = bridge_pid(&temp.path().join("work"));
    drop(agent);
    kill_pid(pid);
    kill_pid(bridge);
}

fn before_quit(log: &Path) -> bool {
    fs::read_to_string(log).unwrap_or_default().contains("quit")
}

fn bridge_pid(work: &Path) -> u32 {
    let text = fs::read_to_string(work.join("server-process.json")).unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("bridgePid").and_then(|v| v.as_u64()))
        .unwrap_or(0) as u32
}

fn agent_log_contains(agent: &Agent, needle: &str) -> bool {
    agent
        .logs_after(0)
        .iter()
        .any(|line| line.message.contains(needle))
}

fn walk_contains(dir: &Path, name: &str, bytes: &[u8]) -> bool {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name)
                && fs::read(&path).ok().as_deref() == Some(bytes)
            {
                return true;
            }
        }
    }
    false
}
