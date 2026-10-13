use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use launcher_core::{
    asset_matches, download_with_resume, extract_zip, format_servers_ini, http_client, inspect_game,
    is_newer, load_catalog, load_servers, parse_web_build, patch_game, prepare_client_launch,
    save_settings, spawn_process, write_manifest, DownloadRequest, GameStatus, InspectRequest,
    InstalledManifest, PatchJob, PlayPlan, ReleaseSource, ServerEntry, Settings,
};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

struct Remote {
    tag: String,
    notes: String,
    zip: Vec<u8>,
    /// When set, `/file.zip` returns this body instead of `zip`.
    raw: Option<Vec<u8>>,
    releases: Option<String>,
    saw_range: bool,
    not_found: bool,
}

fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default();
        for (name, bytes) in files {
            writer.start_file(*name, opts).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

fn remote(tag: &str, zip: Vec<u8>) -> Remote {
    Remote {
        tag: tag.into(),
        notes: String::new(),
        zip,
        raw: None,
        releases: None,
        saw_range: false,
        not_found: false,
    }
}

fn start_remote(initial: Remote) -> (String, Arc<Mutex<Remote>>) {
    let state = Arc::new(Mutex::new(initial));
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let shared = Arc::clone(&state);
    thread::spawn(move || {
        for request in server.incoming_requests() {
            if let Err(err) = respond(port, &shared, request) {
                eprintln!("mock server: {err}");
            }
        }
    });
    (format!("http://127.0.0.1:{port}"), state)
}

fn respond(
    port: u16,
    state: &Mutex<Remote>,
    request: tiny_http::Request,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let url = request.url().to_string();
    let mut remote = state.lock().unwrap();
    if url.contains("/missing") || remote.not_found {
        drop(remote);
        let body = br#"{"message":"Not Found"}"#.to_vec();
        request.respond(tiny_http::Response::from_data(body).with_status_code(404))?;
        return Ok(());
    }
    if url.contains("/releases") {
        let body = if let Some(custom) = &remote.releases {
            custom.clone()
        } else {
            release_json(port, &remote)
        };
        drop(remote);
        let header = tiny_http::Header::from_bytes(b"Content-Type", b"application/json").unwrap();
        request.respond(tiny_http::Response::from_data(body.into_bytes()).with_header(header))?;
        return Ok(());
    }
    if url.contains("/grave") {
        drop(remote);
        let html = b"<span class=\"gm-build\">Build 72, last updated Oct 13, 2026</span>".to_vec();
        request.respond(tiny_http::Response::from_data(html))?;
        return Ok(());
    }
    let payload = if let Some(raw) = &remote.raw {
        raw.clone()
    } else {
        remote.zip.clone()
    };
    let range = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Range"))
        .map(|header| header.value.as_str().to_string());
    if range.is_some() {
        remote.saw_range = true;
    }
    drop(remote);
    if let Some(range) = range {
        let start: usize = range
            .trim()
            .trim_start_matches("bytes=")
            .split('-')
            .next()
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        if start > payload.len() {
            request.respond(tiny_http::Response::from_data(Vec::<u8>::new()).with_status_code(416))?;
            return Ok(());
        }
        let slice = payload[start.min(payload.len())..].to_vec();
        request.respond(tiny_http::Response::from_data(slice).with_status_code(206))?;
        return Ok(());
    }
    request.respond(tiny_http::Response::from_data(payload))?;
    Ok(())
}

fn release_json(port: u16, remote: &Remote) -> String {
    let size = remote.raw.as_ref().map(|r| r.len()).unwrap_or(remote.zip.len());
    serde_json::json!([{
        "tag_name": remote.tag,
        "name": format!("{} – The Brawler's Update", remote.tag),
        "body": remote.notes,
        "draft": false,
        "prerelease": false,
        "html_url": format!("http://127.0.0.1:{port}/release"),
        "assets": [{
            "name": "FridayNightFights_0.9.5_Client_Win64.zip",
            "url": format!("http://127.0.0.1:{port}/file.zip"),
            "browser_download_url": format!("http://127.0.0.1:{port}/file.zip"),
            "size": size,
            "digest": null
        }, {
            "name": "FridayNightFights_Server_Win64.zip",
            "url": format!("http://127.0.0.1:{port}/server.zip"),
            "browser_download_url": format!("http://127.0.0.1:{port}/server.zip"),
            "size": 1
        }]
    }])
    .to_string()
}

fn source(api: &str) -> ReleaseSource {
    ReleaseSource {
        api_base: api.to_string(),
        owner: "uXa-Sloppy".into(),
        repo: "FridayNightFights".into(),
        asset_contains: vec!["Client".into(), "Win64".into()],
        asset_pattern: String::new(),
        include_prereleases: true,
        token: None,
    }
}

fn fnf_game() -> launcher_core::GameDef {
    load_catalog(include_str!("../../../config/games.json"))
        .unwrap()
        .games
        .into_iter()
        .find(|game| game.id == "friday-night-fights")
        .unwrap()
}

fn grave_game(url: String) -> launcher_core::GameDef {
    let mut game = load_catalog(include_str!("../../../config/games.json"))
        .unwrap()
        .games
        .into_iter()
        .find(|game| game.id == "grave-march")
        .unwrap();
    match &mut game.source {
        launcher_core::GameSource::Web { url: slot, .. } => *slot = url,
        _ => panic!("grave march should be a web game"),
    }
    game
}

fn client_script(marker: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\ndir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\nprintf '%s\\n' \"$1\" > \"$dir/{marker}\"\n"
    )
    .into_bytes()
}

#[test]
fn asset_filter_picks_client_win64_zips_only() {
    let contains = vec!["Client".into(), "Win64".into()];
    assert!(asset_matches(
        "FridayNightFights_0.9.5_Client_Win64.zip",
        &contains,
        None
    ));
    assert!(!asset_matches("FridayNightFights_Server_Win64.zip", &contains, None));
    assert!(!asset_matches(
        "FridayNightFights_0.9.5_Client_Linux.zip",
        &contains,
        None
    ));
    assert!(!asset_matches(
        "FridayNightFights_0.9.5_Client_Win64.exe",
        &contains,
        None
    ));
    let pattern = regex::Regex::new(r"^FridayNightFights_.*_Client_Win64\.zip$").unwrap();
    assert!(asset_matches(
        "FridayNightFights_0.9.5_Client_Win64.zip",
        &contains,
        Some(&pattern)
    ));
    assert!(!asset_matches("Other_Client_Win64.zip", &contains, Some(&pattern)));
    assert!(is_newer("v0.9.6", Some("v0.9.5")).unwrap());
}

#[test]
fn servers_ini_matches_the_game_format() {
    let servers = vec![
        ServerEntry {
            id: "dolmar".into(),
            name: "Dolmar's Server".into(),
            host: "71.71.45.128".into(),
            port: 7777,
            default: true,
        },
        ServerEntry {
            id: "localhost".into(),
            name: "Local|host\n".into(),
            host: "127.0.0.1".into(),
            port: 7777,
            default: false,
        },
    ];
    assert_eq!(
        format_servers_ini(&servers),
        "Dolmar's Server | 71.71.45.128 | 7777\nLocal host  | 127.0.0.1 | 7777\n"
    );
    let bundled = load_servers(include_str!("../../../config/servers.json")).unwrap();
    assert_eq!(bundled["friday-night-fights"][0].host, "71.71.45.128");
}

#[tokio::test]
async fn patch_preserves_saves_and_swaps_atomically() {
    let script = client_script("last-launch.txt");
    let old_zip = zip_bytes(&[("FridayNightFights.exe", b"OLD")]);
    let new_zip = zip_bytes(&[
        ("FridayNightFights.exe", script.as_slice()),
        ("Engine/README.txt", b"client 0.9.5\n"),
        ("Saved/SaveGames/default.sav", b"SHIPPED"),
        ("Config/DefaultGame.ini", b"shipped-config"),
    ]);
    let (api, state) = start_remote({
        let mut remote = remote("v0.9.4", old_zip);
        remote.notes = "old notes".into();
        remote
    });
    let root = tempfile::tempdir().unwrap();
    let install = root.path().join("FridayNightFights");
    let downloads = root.path().join("downloads");
    let client = http_client().unwrap();

    let first = patch_game(
        &client,
        &PatchJob {
            source: source(&api),
            install_dir: install.clone(),
            download_dir: downloads.clone(),
            preserve: vec!["Saved/SaveGames".into(), "Saved/Config".into()],
            game_id: "friday-night-fights".into(),
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(first.version, "v0.9.4");
    fs::create_dir_all(install.join("Saved/SaveGames")).unwrap();
    fs::create_dir_all(install.join("Saved/Config/Windows")).unwrap();
    fs::write(install.join("Saved/SaveGames/slot1.sav"), b"HERO").unwrap();
    fs::write(
        install.join("Saved/Config/Windows/GameUserSettings.ini"),
        b"Volume=10",
    )
    .unwrap();
    fs::write(install.join("OnlyOld.txt"), b"do-not-keep").unwrap();

    {
        let mut remote = state.lock().unwrap();
        remote.tag = "v0.9.5".into();
        remote.notes = "- New arena: The Rusty Cage\n".into();
        remote.zip = new_zip;
    }

    let second = patch_game(
        &client,
        &PatchJob {
            source: source(&api),
            install_dir: install.clone(),
            download_dir: downloads.clone(),
            preserve: vec!["Saved/SaveGames".into(), "Saved/Config".into()],
            game_id: "friday-night-fights".into(),
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(second.version, "v0.9.5");
    assert_eq!(
        fs::read(install.join("Saved/SaveGames/slot1.sav")).unwrap(),
        b"HERO"
    );
    assert_eq!(
        fs::read(install.join("Saved/Config/Windows/GameUserSettings.ini")).unwrap(),
        b"Volume=10"
    );
    assert_eq!(
        fs::read(install.join("Saved/SaveGames/default.sav")).unwrap(),
        b"SHIPPED"
    );
    assert_eq!(
        fs::read(install.join("Config/DefaultGame.ini")).unwrap(),
        b"shipped-config"
    );
    assert!(!install.join("OnlyOld.txt").exists());
    assert!(!root.path().join("FridayNightFights.previous").exists());
    assert!(fs::read(&install.join("FridayNightFights.exe")).unwrap().starts_with(b"#!/bin/sh"));

    let servers = load_servers(include_str!("../../../config/servers.json")).unwrap();
    let list = &servers["friday-night-fights"];
    let plan = prepare_client_launch(launcher_core::ClientLaunch {
        install_dir: &install,
        executable_name: "FridayNightFights.exe",
        servers_config_relative: "Config/Servers.ini",
        servers: list,
        selected: &list[0],
    })
    .unwrap();
    let PlayPlan::Process { args, program, .. } = &plan;
    assert_eq!(args, &vec!["71.71.45.128:7777".to_string()]);
    assert_eq!(
        fs::read_to_string(install.join("Config/Servers.ini")).unwrap(),
        "Dolmar's Server | 71.71.45.128 | 7777\nLocalhost | 127.0.0.1 | 7777\n"
    );

    #[cfg(unix)]
    {
        let _ = program;
        spawn_process(&plan).unwrap();
        let marker = install.join("last-launch.txt");
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !marker.exists() {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(fs::read_to_string(marker).unwrap().trim(), "71.71.45.128:7777");
    }
    #[cfg(not(unix))]
    {
        assert!(program.ends_with("FridayNightFights.exe"));
    }
}

#[tokio::test]
async fn corrupt_archive_does_not_replace_the_install() {
    let (api, state) = start_remote({
        let mut remote = remote("v0.9.4", zip_bytes(&[("FridayNightFights.exe", b"OLD")]));
        remote.notes = "ok".into();
        remote
    });
    let root = tempfile::tempdir().unwrap();
    let install = root.path().join("FridayNightFights");
    let client = http_client().unwrap();
    patch_game(
        &client,
        &job(&api, &install, root.path()),
        |_| {},
    )
    .await
    .unwrap();
    fs::write(install.join("OnlyOld.txt"), b"safe").unwrap();

    {
        let mut remote = state.lock().unwrap();
        remote.tag = "v0.9.5".into();
        remote.raw = Some(b"this is not a zip".to_vec());
        remote.zip.clear();
    }
    let err = patch_game(&client, &job(&api, &install, root.path()), |_| {})
        .await
        .unwrap_err();
    assert!(err.to_string().contains("zip") || err.to_string().contains("not a zip"));
    assert_eq!(fs::read(install.join("FridayNightFights.exe")).unwrap(), b"OLD");
    assert_eq!(fs::read(install.join("OnlyOld.txt")).unwrap(), b"safe");
}

#[tokio::test]
async fn download_resumes_from_a_partial_file() {
    let payload = b"0123456789abcdef".repeat(400);
    let (api, state) = start_remote(remote("v0.9.5", payload.clone()));
    let root = tempfile::tempdir().unwrap();
    let dest = root.path().join("asset.partial");
    fs::write(&dest, &payload[..1500]).unwrap();
    let client = http_client().unwrap();
    let url = format!("{api}/file.zip");
    let got = download_with_resume(
        DownloadRequest {
            client: &client,
            url: &url,
            dest: &dest,
            expected_size: payload.len() as u64,
            token: None,
        },
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(got, payload.len() as u64);
    assert_eq!(fs::read(dest).unwrap(), payload);
    assert!(state.lock().unwrap().saw_range, "download did not send a Range header");
}

#[test]
fn zip_slip_is_rejected() {
    let bytes = zip_bytes(&[("../evil.txt", b"nope"), ("ok.txt", b"yes")]);
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("bad.zip");
    fs::write(&archive, bytes).unwrap();
    let dest = root.path().join("stage");
    let err = extract_zip(&archive, &dest).unwrap_err();
    assert!(err.to_string().contains("unsafe") || err.to_string().contains("blocked"));
    assert!(!root.path().join("evil.txt").exists());
}

#[tokio::test]
async fn grave_march_build_line_and_offline_fallback() {
    let html = r#"<div id="unity-build-title">Grave March · <!--BUILD-->
<span class="gm-build">Build 72, last updated Oct 13, 2026
</span>"#;
    let parsed = parse_web_build(html, "").unwrap();
    assert_eq!(parsed.build, "72");

    let (api, _) = start_remote(remote("v0", Vec::new()));
    let client = http_client().unwrap();
    let game = grave_game(format!("{api}/grave"));
    let status: GameStatus = inspect_game(InspectRequest {
        client: &client,
        game: &game,
        manifest: None,
        api_base: &api,
        token: None,
        remote: true,
    })
    .await
    .unwrap();
    assert_eq!(status.web_build.as_deref(), Some("72"));
    assert_eq!(status.web_updated.as_deref(), Some("Oct 13, 2026"));
    assert_eq!(status.state, "ready");
    assert!(status.patch_notes.contains("Build 72"));

    let mut dead = game.clone();
    match &mut dead.source {
        launcher_core::GameSource::Web { url, .. } => *url = "http://127.0.0.1:1/nope".into(),
        _ => unreachable!(),
    }
    let fallback = inspect_game(InspectRequest {
        client: &client,
        game: &dead,
        manifest: None,
        api_base: &api,
        token: None,
        remote: true,
    })
    .await
    .unwrap();
    assert_eq!(fallback.state, "ready");
    assert!(fallback.web_build.is_none());
    assert!(fallback.message.contains("still play"));
}

#[tokio::test]
async fn private_repo_404_keeps_an_installed_game_playable() {
    let (api, state) = start_remote(remote("v0", Vec::new()));
    state.lock().unwrap().not_found = true;
    let client = http_client().unwrap();
    let game = fnf_game();
    let manifest = InstalledManifest {
        game_id: game.id.clone(),
        version: "v0.9.5".into(),
        asset_name: "FridayNightFights_0.9.5_Client_Win64.zip".into(),
        install_path: "unused".into(),
        installed_at: "now".into(),
    };
    let status = inspect_game(InspectRequest {
        client: &client,
        game: &game,
        manifest: Some(&manifest),
        api_base: &format!("{api}/missing"),
        token: None,
        remote: true,
    })
    .await
    .unwrap();
    assert_eq!(status.state, "ready");
    assert_eq!(status.installed_version.as_deref(), Some("v0.9.5"));
    assert!(
        status.message.to_lowercase().contains("private") || status.message.contains("token"),
        "{}",
        status.message
    );
}

#[test]
fn settings_roundtrip_has_no_token_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    let mut settings = Settings::default();
    settings.install_root = r"D:\uXa\Games".into();
    settings.check_on_start = false;
    settings
        .selected_server
        .insert("friday-night-fights".into(), "dolmar".into());
    save_settings(&path, &settings).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.to_lowercase().contains("token"));
    assert!(text.contains("checkOnStart"));
    let loaded = launcher_core::load_settings(&path).unwrap();
    assert!(!loaded.check_on_start);
    assert_eq!(loaded.install_root, r"D:\uXa\Games");
    let manifest_path = dir.path().join("manifests").join("friday-night-fights.json");
    write_manifest(
        &manifest_path,
        &InstalledManifest {
            game_id: "friday-night-fights".into(),
            version: "v0.9.5".into(),
            asset_name: "a.zip".into(),
            install_path: r"D:\uXa\Games\FridayNightFights".into(),
            installed_at: "t".into(),
        },
    )
    .unwrap();
    assert_eq!(
        launcher_core::read_manifest(&manifest_path)
            .unwrap()
            .unwrap()
            .version,
        "v0.9.5"
    );
}

fn job(api: &str, install: &Path, root: &Path) -> PatchJob {
    PatchJob {
        source: source(api),
        install_dir: install.to_path_buf(),
        download_dir: root.join("downloads"),
        preserve: vec!["Saved/SaveGames".into(), "Saved/Config".into()],
        game_id: "friday-night-fights".into(),
    }
}

#[test]
fn nested_executable_is_found_without_walking_saves() {
    let root = tempfile::tempdir().unwrap();
    let install = root.path().join("game");
    let nested = install.join("FridayNightFights/Binaries/Win64");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(install.join("Saved/SaveGames")).unwrap();
    fs::write(nested.join("FridayNightFights.exe"), b"exe").unwrap();
    fs::write(install.join("Saved/SaveGames/FridayNightFights.exe"), b"nope").unwrap();
    let found = launcher_core::find_executable(&install, "FridayNightFights.exe").unwrap();
    assert_eq!(found, nested.join("FridayNightFights.exe"));
}
