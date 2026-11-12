//! 0.3.12 keeps game installs on a signed command. Startup cancels a release
//! that 0.3.11 staged for 03:00. A check reports and does not download.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use launcher_core::agent_exchange;

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
use uxa_server_agent::{serve_listener, AccountServerConfig, Agent, AgentConfig, AutoUpdateConfig};

fn zip_counter() -> &'static AtomicUsize {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    &COUNT
}

fn start_remote() -> String {
    zip_counter().store(0, Ordering::SeqCst);
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let releases = format!(
        r#"[{{"tag_name":"v0.10.1","name":"Live","draft":false,"prerelease":false,"assets":[{{"name":"FridayNightFights-Server-Win64.zip","browser_download_url":"http://127.0.0.1:{port}/server.zip","size":4}}]}}]"#
    );
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url().to_string();
            let (status, body, kind) = if url.contains("server.zip") {
                zip_counter().fetch_add(1, Ordering::SeqCst);
                (500, b"no download".to_vec(), "text/plain")
            } else if url.contains("/releases") {
                (200, releases.as_bytes().to_vec(), "application/json")
            } else {
                (404, b"missing".to_vec(), "text/plain")
            };
            let response = tiny_http::Response::from_data(body)
                .with_status_code(status)
                .with_header(
                    tiny_http::Header::from_bytes(b"Content-Type", kind.as_bytes()).unwrap(),
                );
            let _ = request.respond(response);
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn config(root: &Path, api: &str, listen: &str) -> AgentConfig {
    let mut config = AgentConfig {
        install_dir: root.join("FridayNightFightsServer"),
        executable: "FridayNightFightsServer.exe".into(),
        port: 7777,
        args: Vec::new(),
        preserve: vec!["Saved".into()],
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
        quit_wait_secs: 2,
        require_tcp: false,
        tls_cert: Path::new("").into(),
        tls_key: Path::new("").into(),
        backup_dir: root.join("backups"),
        work_dir: root.join("work"),
        log_dir: root.join("logs"),
        secret: "test-secret-not-for-production".into(),
        auto_update: AutoUpdateConfig::production(),
        account_server: AccountServerConfig::disabled(),
        source_path: root.join("agent.json"),
    };
    config.finish_paths().unwrap();
    config
}

#[tokio::test]
async fn startup_cancels_a_staged_0300_install() {
    let temp = tempfile::tempdir().unwrap();
    let work = temp.path().join("work");
    fs::create_dir_all(work.join("staged-server")).unwrap();
    fs::write(work.join("staged-server").join("payload.bin"), b"STAGED").unwrap();
    fs::write(
        work.join("staged-server.json"),
        r#"{"tag":"v0.10.0","asset":"FridayNightFights-Server-Win64.zip"}"#,
    )
    .unwrap();
    fs::write(
        work.join("auto-update-state.json"),
        r#"{"stagedVersion":"v0.10.0","lastMaintenanceDate":"2026-11-11","lastUpdateResult":"staged v0.10.0"}"#,
    )
    .unwrap();
    let agent = Agent::new(config(temp.path(), "http://127.0.0.1:9", "127.0.0.1:9")).unwrap();
    assert!(!work.join("staged-server").exists());
    assert!(!work.join("staged-server.json").exists());
    let state: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(work.join("auto-update-state.json")).unwrap())
            .unwrap();
    assert_eq!(state["stagedVersion"], "");
    assert_eq!(state["pendingInstallCancelled"], true);
    assert_eq!(state["cancelledStagedVersion"], "v0.10.0");
    let result = state["lastUpdateResult"].as_str().unwrap();
    assert!(
        result.contains("cancelled pending 03:00 install of v0.10.0"),
        "{result}"
    );
    assert!(result.contains("on-demand"), "{result}");
    let status = agent.status().await;
    assert_eq!(status.update_mode, "on-demand");
    assert_eq!(status.staged_version, "");
    assert!(status.next_restart_at.is_empty());
    assert_eq!(status.check_interval_minutes, 10);
}

#[tokio::test]
async fn check_config_and_diag_do_not_escape_scope() {
    let temp = tempfile::tempdir().unwrap();
    let api = start_remote();
    let install = temp.path().join("FridayNightFightsServer");
    fs::create_dir_all(install.join("Saved/Crashes/one")).unwrap();
    fs::write(install.join("Saved/Crashes/one/crash.dmp"), b"DMPDATA").unwrap();
    fs::create_dir_all(temp.path().join("logs")).unwrap();
    fs::write(
        temp.path().join("logs/server.log"),
        b"bad-start token=supersecret host 198.51.100.20\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("agent.json"),
        format!(
            r#"{{"installDir":"{}","listen":"127.0.0.1:9","port":7777,"owner":"uXa-Sloppy","repo":"FridayNightFights-Releases","autoUpdate":{{"enabled":true,"dailyRestart":true,"serverEnabled":true}}}}"#,
            install.display().to_string().replace('\\', "\\\\")
        ),
    )
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let agent = Agent::new(config(temp.path(), &api, &addr.to_string())).unwrap();
    let secret = agent.config.secret.clone();
    tokio::spawn(async move {
        serve_listener(agent, listener).await.unwrap();
    });
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();

    let unsigned = http
        .post(format!("{base}/v1/check"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), 401);

    let status: serde_json::Value = serde_json::from_str(
        &call(&http, &base, &secret, "POST", "/v1/check", b"{}")
            .await
            .unwrap(),
    )
    .unwrap();
    let result = status["lastCheckResult"].as_str().unwrap();
    assert!(result.contains("not installing"), "{result}");
    assert!(result.contains("v0.10.1"), "{result}");
    assert_eq!(zip_counter().load(Ordering::SeqCst), 0);
    assert!(!temp.path().join("work/staged-server").exists());
    assert!(!temp.path().join("work/downloads").exists());

    for (body, needle) in [
        (r#"{"installDir":"D:\\Other"}"#, "installDir"),
        (r#"{"listen":"0.0.0.0:1"}"#, "listen"),
        (r#"{"port":9}"#, "port"),
        (r#"{"owner":"someone-else"}"#, "uXa-Sloppy"),
        (r#"{"repo":"../evil"}"#, "repository"),
        (r#"{"repo":"https://evil.example/x"}"#, "repository"),
        (r#"{"checkIntervalMinutes":4}"#, "5 and 15"),
        (r#"{"checkIntervalMinutes":16}"#, "5 and 15"),
        (r#"{"updateMode":"auto"}"#, "on-demand"),
        (r#"{"onFailedStart":"rollback"}"#, "stay"),
    ] {
        let err = call(
            &http,
            &base,
            &secret,
            "PATCH",
            "/v1/config",
            body.as_bytes(),
        )
        .await
        .expect_err(body);
        assert!(err.contains(needle), "{body} -> {err}");
    }

    let patched: serde_json::Value = serde_json::from_str(
        &call(
            &http,
            &base,
            &secret,
            "PATCH",
            "/v1/config",
            br#"{"updateMode":"on-demand","onFailedStart":"stay","checkIntervalMinutes":15,"repo":"FridayNightFights-Releases"}"#,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(patched["ok"], true);
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(temp.path().join("agent.json")).unwrap()).unwrap();
    assert_eq!(saved["listen"], "127.0.0.1:9");
    assert_eq!(saved["port"], 7777);
    assert_eq!(saved["owner"], "uXa-Sloppy");
    assert_eq!(saved["repo"], "FridayNightFights-Releases");
    assert_eq!(saved["autoUpdate"]["checkIntervalMinutes"], 15);
    assert_eq!(saved["autoUpdate"]["updateMode"], "on-demand");
    assert_eq!(saved["autoUpdate"]["dailyRestart"], true);
    assert!(saved.get("secret").is_none());

    let logs_err = call(
        &http,
        &base,
        &secret,
        "GET",
        "/v1/diag/logs?source=../../etc/passwd",
        b"",
    )
    .await
    .expect_err("path source");
    assert!(
        logs_err.contains("source")
            || logs_err.contains("400")
            || logs_err.contains("server, agent"),
        "{logs_err}"
    );

    let logs: serde_json::Value = serde_json::from_str(
        &call(
            &http,
            &base,
            &secret,
            "GET",
            "/v1/diag/logs?source=server&lines=40",
            b"",
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let tail = logs["tail"].as_str().unwrap();
    assert!(tail.contains("bad-start"), "{tail}");
    assert!(!tail.contains("supersecret"), "{tail}");
    assert!(!tail.contains("198.51.100.20"), "{tail}");

    let crashes: serde_json::Value = serde_json::from_str(
        &call(&http, &base, &secret, "GET", "/v1/diag/crashes", b"")
            .await
            .unwrap(),
    )
    .unwrap();
    let id = crashes["crashes"][0]["id"].as_str().unwrap();
    let download = call(
        &http,
        &base,
        &secret,
        "GET",
        &format!("/v1/diag/crashes/download?id={id}"),
        b"",
    )
    .await
    .unwrap();
    assert_eq!(download, "DMPDATA");
    let missing = call(
        &http,
        &base,
        &secret,
        "GET",
        "/v1/diag/crashes/download?id=../etc/passwd",
        b"",
    )
    .await
    .expect_err("traversal id");
    assert!(
        missing.contains("unknown") || missing.contains("404"),
        "{missing}"
    );

    let events: serde_json::Value = serde_json::from_str(
        &call(&http, &base, &secret, "GET", "/v1/diag/events", b"")
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(events["source"], "Application");
    if cfg!(windows) {
        assert!(events["events"].is_array());
    } else {
        assert_eq!(events["available"], false);
    }

    fs::create_dir_all(install.join("FridayNightFights/Binaries/Win64")).unwrap();
    fs::write(
        install.join("FridayNightFights/Binaries/Win64/FridayNightFightsServer-Win64-Shipping.exe"),
        b"MZ",
    )
    .unwrap();
    fs::write(install.join("Saved/skip-Win64-Shipping.exe"), b"no").unwrap();
    let net: serde_json::Value = serde_json::from_str(
        &call(
            &http,
            &base,
            &secret,
            "GET",
            "/v1/diag/net?expect=203.0.113.50",
            b"",
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(net["port"], 7777);
    assert!(net["firewall"]["rules"].is_array(), "{net}");
    assert!(net["listening"]["endpoints"].is_array(), "{net}");
    assert!(net["lan"]["addresses"].is_array(), "{net}");
    let rendered = net.to_string();
    assert!(
        rendered.contains("FridayNightFightsServer-Win64-Shipping.exe"),
        "{rendered}"
    );
    assert!(!rendered.contains("skip-Win64-Shipping"), "{rendered}");
    match &net["publicIpMatchesExpected"] {
        serde_json::Value::Null => assert!(net["publicIp"].is_null(), "{net}"),
        serde_json::Value::Bool(matched) => {
            let ip = net["publicIp"].as_str().unwrap();
            assert!(ip.contains('.'), "{ip}");
            assert!(!ip.contains("redacted"), "{ip}");
            assert_eq!(*matched, ip == "203.0.113.50", "{net}");
        }
        other => panic!("publicIpMatchesExpected should be bool or null, got {other}"),
    }
    if let Some(addresses) = net["lan"]["addresses"].as_array() {
        for addr in addresses {
            let ip = addr["address"].as_str().unwrap_or("");
            assert!(!ip.contains("redacted"), "{ip}");
            assert!(!ip.starts_with("127."), "{ip}");
            assert!(!ip.starts_with("169.254."), "{ip}");
        }
    }
    let bad_port = call(&http, &base, &secret, "GET", "/v1/diag/net?port=9", b"")
        .await
        .expect_err("port");
    assert!(
        bad_port.contains("7777") || bad_port.contains("port"),
        "{bad_port}"
    );
    let bad_expect = call(
        &http,
        &base,
        &secret,
        "GET",
        "/v1/diag/net?expect=not-an-ip",
        b"",
    )
    .await
    .expect_err("expect");
    assert!(
        bad_expect.contains("IPv4") || bad_expect.contains("expect"),
        "{bad_expect}"
    );
}
