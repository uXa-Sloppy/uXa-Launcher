//! Server deploy from a split (>2 GB style) zip, and the managed account server:
//! deploy its package, health-check it, add an invite key by hash, sign up, and
//! read it all back from GET /v1/monitor.
#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use launcher_core::{agent_exchange, split_file, AgentAction};
use sha2::Digest;
use uxa_server_agent::{serve_listener, AccountServerConfig, Agent, AgentConfig, AutoUpdateConfig};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

fn zip_bytes(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default()
            .unix_permissions(0o755)
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in files {
            writer.start_file(*name, opts).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

fn good_script() -> String {
    "#!/bin/sh\n\
     dir=$(CDPATH= cd -- \"$(dirname \"$0\")\" && pwd)\n\
     mkdir -p \"$dir/FridayNightFights/Saved/Logs\"\n\
     printf '%s\\n' '{\"playerCount\":1,\"players\":[{\"account\":\"Dave\",\"character\":\"Sloppy\"}],\"map\":\"Purgatory\"}' > \"$dir/FridayNightFights/Saved/uxa-status.json\"\n\
     printf 'LogNet: Join succeeded: Dave\\n' > \"$dir/FridayNightFights/Saved/Logs/FridayNightFights.log\"\n\
     while IFS= read -r line; do case \"$line\" in quit*) exit 0 ;; esac; done\n"
        .to_string()
}

fn serve_files(files: HashMap<String, Vec<u8>>, tag: &'static str) -> String {
    let files = Arc::new(Mutex::new(files));
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let b2 = base.clone();
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url().to_string();
            let files = files.lock().unwrap();
            if url.contains("/releases") {
                let assets: Vec<String> = files
                    .iter()
                    .map(|(n, d)| format!(r#"{{"name":"{n}","url":"{b2}/dl/{n}","browser_download_url":"{b2}/dl/{n}","size":{}}}"#, d.len()))
                    .collect();
                let body = format!(r#"[{{"tag_name":"{tag}","name":"t","draft":false,"prerelease":false,"assets":[{}]}}]"#, assets.join(","));
                let _ = request.respond(tiny_http::Response::from_data(body.into_bytes()));
                continue;
            }
            let name = url.trim_start_matches("/dl/");
            match files.get(name) {
                Some(d) => {
                    let _ = request.respond(tiny_http::Response::from_data(d.clone()));
                }
                None => {
                    let _ = request.respond(tiny_http::Response::from_data(b"no".to_vec()).with_status_code(404));
                }
            }
        }
    });
    base
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn config(root: &Path, api: &str, listen: &str, account_port: u16) -> AgentConfig {
    let mut accounts = AccountServerConfig::disabled();
    accounts.enabled = true;
    accounts.port = account_port;
    accounts.host = "127.0.0.1".into();
    accounts.python = "python3".into();
    let mut config = AgentConfig {
        install_dir: root.join("FridayNightFightsServer"),
        executable: "FridayNightFightsServer.exe".into(),
        port: 7777,
        args: Vec::new(),
        preserve: vec!["Saved".into(), "Config".into()],
        owner: "uXa-Sloppy".into(),
        repo: "FridayNightFights-Releases".into(),
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
        auto_update: AutoUpdateConfig::disabled(),
        account_server: accounts,
    };
    config.finish_paths().unwrap();
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn split_server_deploy_and_managed_account_server() {
    let temp = tempfile::tempdir().unwrap();
    // Server zip, split into small parts (simulates a >2 GB server package).
    let mut filler = vec![0u8; 30_000];
    let mut x: u32 = 7;
    for b in filler.iter_mut() {
        x = x.wrapping_mul(1103515245).wrapping_add(12345);
        *b = (x >> 16) as u8;
    }
    let server_zip = zip_bytes(&[
        ("FridayNightFightsServer.exe", good_script().into_bytes()),
        ("FridayNightFights/Content/Paks/big.ucas", filler),
    ]);
    let server_name = "FNF_0.9.9_Server_Win64.zip";
    let src = temp.path().join(server_name);
    fs::write(&src, &server_zip).unwrap();
    let parts = temp.path().join("parts");
    let manifest = split_file(&src, &parts, 10_000).unwrap();
    let mut files = HashMap::new();
    for p in &manifest.parts {
        files.insert(p.name.clone(), fs::read(parts.join(&p.name)).unwrap());
    }
    let mname = format!("{server_name}.parts.json");
    files.insert(mname.clone(), fs::read(parts.join(&mname)).unwrap());

    // Account server package: the real FNF Tools scripts (python3 on PATH in tests).
    let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/account_server/Tools");
    let mut acct = Vec::new();
    for f in ["fnf_account_server.py", "fnf_account_store.py", "fnf_invite_key.py"] {
        acct.push((format!("Tools/{f}"), fs::read(tools.join(f)).unwrap()));
    }
    let acct_refs: Vec<(&str, Vec<u8>)> = acct.iter().map(|(n, d)| (n.as_str(), d.clone())).collect();
    files.insert("FNF_0.9.9_Accounts_Win64.zip".into(), zip_bytes(&acct_refs));
    let api = serve_files(files, "v0.9.9");

    let account_port = free_port();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let agent = Agent::new(config(temp.path(), &api, &addr.to_string(), account_port)).unwrap();
    let secret = agent.config.secret.clone();
    let agent2 = agent.clone();
    tokio::spawn(async move { serve_listener(agent2, listener).await.unwrap() });
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();
    let call = |method: &'static str, path: &'static str, body: Vec<u8>| {
        let http = http.clone();
        let base = base.clone();
        let secret = secret.clone();
        async move { agent_exchange(&http, &base, &secret, method, path, &body).await.map_err(|e| e.to_string()) }
    };

    let deployed: AgentAction = serde_json::from_str(&call("POST", "/v1/deploy", br#"{"tag":"v0.9.9"}"#.to_vec()).await.unwrap()).unwrap();
    assert!(deployed.ok && deployed.status.online);
    assert_eq!(deployed.status.version, "v0.9.9");
    assert_eq!(deployed.status.player_count, Some(1));

    let acc: serde_json::Value = serde_json::from_str(&call("POST", "/v1/accounts/deploy", br#"{"tag":"v0.9.9"}"#.to_vec()).await.unwrap()).unwrap();
    assert_eq!(acc["accounts"]["healthy"], true, "{acc}");
    assert_eq!(acc["accounts"]["version"], "v0.9.9");

    let key = "FNF-TEST-ABCD-EFGH";
    let digest = hex::encode(sha2::Sha256::digest(key.as_bytes()));
    let body = serde_json::to_vec(&serde_json::json!({"keySha256": digest, "note": "tester"})).unwrap();
    call("POST", "/v1/accounts/invite", body.clone()).await.unwrap();
    let dup = call("POST", "/v1/accounts/invite", body).await.unwrap_err();
    assert!(dup.contains("already"), "{dup}");
    let bad = call("POST", "/v1/accounts/invite", br#"{"keySha256":"nope"}"#.to_vec()).await.unwrap_err();
    assert!(bad.contains("64 hex"), "{bad}");

    // A player signs up with the plaintext key against the account server port.
    let signup = http
        .post(format!("http://127.0.0.1:{account_port}/v1/signup"))
        .json(&serde_json::json!({"key": key.to_lowercase(), "name": "Tester", "password": "secret1"}))
        .send()
        .await
        .unwrap();
    assert!(signup.status().is_success(), "{}", signup.text().await.unwrap());

    // 0.3.10: the same sign-up flow through the agent's public proxy (no admin signature), in
    // the exact path a 0.10.0 game client sends for -FNFAccountServer=<agent>:<port>/fnf-accounts:<port>.
    let port = addr.port();
    let health = http.get(format!("{base}/fnf-accounts/v1/health")).send().await.unwrap();
    assert!(health.status().is_success());
    let key2 = "FNF-TEST-WXYZ-PROX";
    let digest2 = hex::encode(sha2::Sha256::digest(key2.as_bytes()));
    call("POST", "/v1/accounts/invite", serde_json::to_vec(&serde_json::json!({"keySha256": digest2, "note": "proxy"})).unwrap())
        .await
        .unwrap();
    let proxied = http
        .post(format!("{base}/fnf-accounts:{port}/v1/signup"))
        .json(&serde_json::json!({"key": key2, "name": "Proxied", "password": "secret2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(proxied.status().as_u16(), 200, "{}", proxied.text().await.unwrap());
    let again = http
        .post(format!("{base}/fnf-accounts/v1/signup"))
        .json(&serde_json::json!({"key": key2, "name": "Proxied2", "password": "secret2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status().as_u16(), 400, "a used key is rejected by the account server through the proxy");
    let unknown = http.get(format!("{base}/fnf-accounts/v1/accounts")).send().await.unwrap();
    assert_eq!(unknown.status().as_u16(), 404);
    // Admin routes still need a signature.
    let unsigned = http.get(format!("{base}/v1/monitor")).send().await.unwrap();
    assert_eq!(unsigned.status().as_u16(), 401);

    let mon: serde_json::Value = serde_json::from_str(&call("GET", "/v1/monitor", Vec::new()).await.unwrap()).unwrap();
    assert_eq!(mon["players"]["count"], 1, "{mon}");
    assert_eq!(mon["players"]["players"][0]["character"], "Sloppy");
    assert_eq!(mon["accounts"]["accounts"], 6);
    assert_eq!(mon["accounts"]["signups"], 2);
    assert_eq!(mon["status"]["online"], true);
    assert!(mon["system"]["memTotalMb"].as_u64().unwrap_or(0) > 0, "{}", mon["system"]);
    assert!(mon["events"].as_array().unwrap().iter().any(|e| e["kind"] == "deploy"));
    // accounts.txt lives in the game Saved tree, so deploy keeps it and backups include it.
    let accounts_file = temp.path().join("FridayNightFightsServer/FridayNightFights/Saved/Accounts/accounts.txt");
    assert!(accounts_file.is_file());

    // Account server dies -> restart route brings it back.
    let restarted: serde_json::Value = serde_json::from_str(&call("POST", "/v1/accounts/restart", Vec::new()).await.unwrap()).unwrap();
    assert_eq!(restarted["accounts"]["healthy"], true);

    // Redeploying the game keeps Saved/Accounts.
    call("POST", "/v1/deploy", br#"{"tag":"v0.9.9"}"#.to_vec()).await.unwrap();
    assert!(accounts_file.is_file(), "accounts must survive a game deploy");

    // Restore a Saved backup: the deploy above made one; the restore backs up first, then swaps.
    let mon: serde_json::Value = serde_json::from_str(&call("GET", "/v1/monitor", Vec::new()).await.unwrap()).unwrap();
    let backup = mon["backups"].as_array().unwrap().iter().find_map(|b| {
        let n = b["name"].as_str()?;
        (n.len() == 15 && !n.starts_with("accounts")).then(|| n.to_string())
    }).expect("a Saved backup");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let body = serde_json::to_vec(&serde_json::json!({"name": backup})).unwrap();
    let restored: AgentAction = serde_json::from_str(&call("POST", "/v1/backups/restore", body).await.unwrap()).unwrap();
    assert!(restored.ok && restored.status.online);
    let bad = call("POST", "/v1/backups/restore", br#"{"name":"../etc"}"#.to_vec()).await.unwrap_err();
    assert!(bad.contains("YYYYMMDD"), "{bad}");

    agent.accounts_stop().await;
    let _ = call("POST", "/v1/stop", Vec::new()).await;
}
