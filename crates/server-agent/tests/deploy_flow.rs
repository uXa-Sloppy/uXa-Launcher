//! Deploy, preserve saves, roll back a build that dies, and reject a bad signature.
//! The "dedicated server" is a shell script inside a zip served by a local GitHub stand-in.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use launcher_core::{agent_exchange, sign_request, AgentAction, AgentLogLine, AgentReleaseSummary, AgentStatus};
use uxa_server_agent::{serve_listener, Agent, AgentConfig};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

struct Remote {
    good: Vec<u8>,
    bad: Vec<u8>,
    client: Vec<u8>,
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
         dir=$(CDPATH= cd -- \"$(dirname \"$0\")\" && pwd)\n\
         mkdir -p \"$dir/Saved\"\n\
         printf '%s\\n' '{{\"playerCount\":2}}' > \"$dir/Saved/uxa-status.json\"\n\
         while IFS= read -r line; do\n\
           printf '%s\\n' \"$line\" >> '{log}'\n\
           case \"$line\" in\n\
             quit*) exit 0 ;;\n\
           esac\n\
         done\n"
    )
}

fn bad_script(log: &str) -> String {
    format!("#!/bin/sh\nprintf 'bad-start\\n' >> '{log}'\nexit 1\n")
}

fn start_remote(remote: Remote) -> String {
    let state = Arc::new(Mutex::new(remote));
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url().to_string();
            let remote = state.lock().unwrap();
            let (status, body, content_type) = if url.contains("/releases") {
                let body = releases_json(port, &remote);
                (200, body.into_bytes(), "application/json")
            } else if url.contains("/good.zip") {
                (200, remote.good.clone(), "application/octet-stream")
            } else if url.contains("/bad.zip") {
                (200, remote.bad.clone(), "application/octet-stream")
            } else if url.contains("/client.zip") {
                (200, remote.client.clone(), "application/octet-stream")
            } else {
                (404, b"missing".to_vec(), "text/plain")
            };
            drop(remote);
            let response = tiny_http::Response::from_data(body)
                .with_status_code(status)
                .with_header(tiny_http::Header::from_bytes(b"Content-Type", content_type.as_bytes()).unwrap());
            let _ = request.respond(response);
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn releases_json(port: u16, remote: &Remote) -> String {
    let base = format!("http://127.0.0.1:{port}");
    format!(
        r#"[
          {{
            "tag_name": "v1.2.0",
            "name": "Good server",
            "draft": false,
            "prerelease": false,
            "assets": [
              {{
                "name": "FridayNightFights-Client-Win64.zip",
                "url": "{base}/client.zip",
                "browser_download_url": "{base}/client.zip",
                "size": {client}
              }},
              {{
                "name": "FridayNightFights-Server-Win64.zip",
                "url": "{base}/good.zip",
                "browser_download_url": "{base}/good.zip",
                "size": {good}
              }}
            ]
          }},
          {{
            "tag_name": "v9.9.9-broken",
            "name": "Broken server",
            "draft": false,
            "prerelease": true,
            "assets": [
              {{
                "name": "FridayNightFights-Server-Win64.zip",
                "url": "{base}/bad.zip",
                "browser_download_url": "{base}/bad.zip",
                "size": {bad}
              }}
            ]
          }}
        ]"#,
        client = remote.client.len(),
        good = remote.good.len(),
        bad = remote.bad.len(),
    )
}

fn test_config(root: &Path, api: &str, listen: &str) -> AgentConfig {
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
        listen: listen.into(),
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
    };
    config.finish_paths().unwrap();
    config
}

async fn call(
    client: &reqwest::Client,
    base: &str,
    secret: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<String, String> {
    agent_exchange(client, base, secret, method, path, body)
        .await
        .map_err(|err| err.to_string())
}

fn walk_has(dir: &Path, name: &str) -> bool {
    if !dir.exists() {
        return false;
    }
    for entry in walkdir_simple(dir) {
        if entry.file_name().and_then(|n| n.to_str()) == Some(name) {
            return true;
        }
    }
    false
}

fn walkdir_simple(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[tokio::test]
async fn deploy_preserves_saves_and_rolls_back() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("events.log");
    let log_text = log.display().to_string();
    let good = zip_bytes(&[(
        "FridayNightFightsServer.exe",
        good_script(&log_text).as_bytes(),
    )]);
    let bad = zip_bytes(&[(
        "FridayNightFightsServer.exe",
        bad_script(&log_text).as_bytes(),
    )]);
    let client_zip = zip_bytes(&[("FridayNightFights.exe", b"client")]);
    let api = start_remote(Remote {
        good,
        bad,
        client: client_zip,
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let agent = Agent::new(test_config(temp.path(), &api, &addr.to_string())).unwrap();
    let secret = agent.config.secret.clone();
    tokio::spawn(async move {
        serve_listener(agent, listener).await.unwrap();
    });
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();

    let ts = "1";
    let forged = sign_request("wrong-key", ts, "POST", "/v1/deploy", b"{}").unwrap();
    let rejected = http
        .post(format!("{base}/v1/deploy"))
        .header("X-Uxa-Timestamp", ts)
        .header("X-Uxa-Signature", forged)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), 401);
    assert!(!temp.path().join("FridayNightFightsServer").exists());

    let deployed = call(&http, &base, &secret, "POST", "/v1/deploy", b"{}")
        .await
        .unwrap();
    let action: AgentAction = serde_json::from_str(&deployed).unwrap();
    assert!(action.ok);
    assert_eq!(action.status.version, "v1.2.0");
    assert!(action.status.online);
    assert_eq!(action.status.port, 7777);
    assert_eq!(action.status.player_count, Some(2));

    let install = temp.path().join("FridayNightFightsServer");
    fs::create_dir_all(install.join("Saved/Worlds")).unwrap();
    fs::write(install.join("Saved/Worlds/island.sav"), b"WORLD").unwrap();
    fs::create_dir_all(install.join("Config")).unwrap();
    fs::write(install.join("Config/ServerSettings.ini"), b"TickRate=30").unwrap();

    let releases: Vec<AgentReleaseSummary> =
        serde_json::from_str(&call(&http, &base, &secret, "GET", "/v1/releases", b"").await.unwrap())
            .unwrap();
    assert!(releases.iter().any(|r| r.tag == "v1.2.0" && r.asset_name.contains("Server")));
    assert!(releases.iter().all(|r| !r.asset_name.contains("Client")));
    assert!(releases.iter().any(|r| r.tag == "v9.9.9-broken"));

    let broken = call(
        &http,
        &base,
        &secret,
        "POST",
        "/v1/deploy",
        br#"{"tag":"v9.9.9-broken"}"#,
    )
    .await;
    let err = broken.expect_err("broken build must fail the deploy");
    assert!(err.contains("Previous build restored"), "{err}");

    let status: AgentStatus =
        serde_json::from_str(&call(&http, &base, &secret, "GET", "/v1/status", b"").await.unwrap())
            .unwrap();
    assert_eq!(status.version, "v1.2.0");
    assert!(status.online, "previous build should be running after rollback");
    assert_eq!(
        fs::read(install.join("Saved/Worlds/island.sav")).unwrap(),
        b"WORLD"
    );
    assert_eq!(
        fs::read(install.join("Config/ServerSettings.ini")).unwrap(),
        b"TickRate=30"
    );
    assert!(
        walk_has(&temp.path().join("backups"), "island.sav"),
        "Saved backup should keep the world"
    );

    let events = fs::read_to_string(&log).unwrap();
    assert!(events.contains("start args"), "{events}");
    assert!(events.contains("-port=7777"), "{events}");
    assert!(events.contains("say uXa:"), "{events}");
    assert!(events.contains("quit"), "{events}");
    assert!(events.contains("bad-start"), "{events}");

    let restarted = call(&http, &base, &secret, "POST", "/v1/restart", b"")
        .await
        .unwrap();
    let restarted: AgentAction = serde_json::from_str(&restarted).unwrap();
    assert!(restarted.status.online);

    let stopped = call(&http, &base, &secret, "POST", "/v1/stop", b"")
        .await
        .unwrap();
    let stopped: AgentAction = serde_json::from_str(&stopped).unwrap();
    assert!(!stopped.status.online);
    assert_eq!(stopped.status.version, "v1.2.0");

    tokio::time::sleep(Duration::from_millis(100)).await;
    let logs: serde_json::Value =
        serde_json::from_str(&call(&http, &base, &secret, "GET", "/v1/logs?after=0", b"").await.unwrap())
            .unwrap();
    let lines: Vec<AgentLogLine> =
        serde_json::from_value(logs.get("lines").cloned().unwrap_or(serde_json::Value::Null))
            .unwrap_or_default();
    let text = lines
        .iter()
        .map(|line| line.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("backed up Saved"), "{text}");
    assert!(text.contains("broadcast warning"), "{text}");
    assert!(text.contains("rolling back"), "{text}");
    assert!(text.contains("rejected"), "{text}");
}

#[test]
fn config_refuses_a_secret_in_json() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent.json");
    fs::write(
        &path,
        r#"{"installDir":"C:\\Games\\FNF","secret":"nope"}"#,
    )
    .unwrap();
    std::env::set_var("UXA_AGENT_CONFIG", &path);
    std::env::set_var("UXA_AGENT_SECRET", "from-env");
    let err = uxa_server_agent::load_config().expect_err("secret in json must be refused");
    assert!(err.contains("secret"), "{err}");
    std::env::remove_var("UXA_AGENT_CONFIG");
    std::env::remove_var("UXA_AGENT_SECRET");
}
