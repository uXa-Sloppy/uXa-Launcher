//! Client patch from a release whose zip is split into parts (the >2 GB case),
//! simulated with a tiny part size. One part is served corrupt on the first
//! request so the per-part retry is exercised, and every download honours Range.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread;

use launcher_core::{patch_game, split_file, PatchJob, ReleaseSource};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

fn zip_bytes(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in files {
            writer.start_file(*name, opts).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }
    cursor.into_inner()
}

struct Files {
    by_name: HashMap<String, Vec<u8>>,
    corrupt_once: Option<String>,
    hits: HashMap<String, u32>,
}

fn serve(files: Files, releases: impl Fn(&str) -> String + Send + 'static) -> String {
    let state = Arc::new(Mutex::new(files));
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let base2 = base.clone();
    thread::spawn(move || {
        for request in server.incoming_requests() {
            let url = request.url().to_string();
            if url.contains("/releases") {
                let body = releases(&base2);
                let _ = request.respond(tiny_http::Response::from_data(body.into_bytes()));
                continue;
            }
            let name = url.trim_start_matches("/dl/").to_string();
            let mut st = state.lock().unwrap();
            let Some(mut data) = st.by_name.get(&name).cloned() else {
                let _ = request.respond(tiny_http::Response::from_data(b"missing".to_vec()).with_status_code(404));
                continue;
            };
            *st.hits.entry(name.clone()).or_default() += 1;
            if st.corrupt_once.as_deref() == Some(name.as_str()) {
                st.corrupt_once = None;
                data[0] ^= 0xFF;
            }
            drop(st);
            let range = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Range"))
                .map(|h| h.value.as_str().to_string());
            if let Some(range) = range {
                let start: usize = range.trim_start_matches("bytes=").trim_end_matches('-').parse().unwrap_or(0);
                if start >= data.len() {
                    let _ = request.respond(tiny_http::Response::from_data(Vec::<u8>::new()).with_status_code(416));
                    continue;
                }
                let _ = request.respond(tiny_http::Response::from_data(data[start..].to_vec()).with_status_code(206));
                continue;
            }
            let _ = request.respond(tiny_http::Response::from_data(data));
        }
    });
    base
}

fn asset_json(base: &str, name: &str, size: usize) -> String {
    format!(r#"{{"name":"{name}","url":"{base}/dl/{name}","browser_download_url":"{base}/dl/{name}","size":{size}}}"#)
}

fn job(api: &str, root: &std::path::Path) -> PatchJob {
    PatchJob {
        source: ReleaseSource {
            api_base: api.into(),
            owner: "uXa-Sloppy".into(),
            repo: "FridayNightFights-Releases".into(),
            asset_contains: vec!["Client".into(), "Win64".into()],
            asset_pattern: String::new(),
            include_prereleases: true,
            token: None,
        },
        install_dir: root.join("games/FridayNightFights"),
        download_dir: root.join("downloads"),
        preserve: vec!["Saved/SaveGames".into()],
        game_id: "friday-night-fights".into(),
    }
}

#[tokio::test]
async fn split_client_release_downloads_verifies_joins_and_installs() {
    let temp = tempfile::tempdir().unwrap();
    // Incompressible payload so the zip really spans several parts.
    let mut pak = vec![0u8; 50_000];
    let mut x: u32 = 12345;
    for b in pak.iter_mut() {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *b = x as u8;
    }
    let zip = zip_bytes(&[
        ("FridayNightFights.exe", b"client-v2".to_vec()),
        ("FridayNightFights/Content/Paks/FridayNightFights-Windows.ucas", pak.clone()),
    ]);
    let zip_name = "FNF_0.9.9_Client_Win64.zip";
    let src = temp.path().join(zip_name);
    fs::write(&src, &zip).unwrap();
    let parts_dir = temp.path().join("parts");
    // 16 KB parts stand in for 1.9 GB parts.
    let manifest = split_file(&src, &parts_dir, 16 * 1024).unwrap();
    assert!(manifest.parts.len() >= 4, "{}", manifest.parts.len());

    let mut by_name = HashMap::new();
    let mut assets = Vec::new();
    let manifest_name = format!("{zip_name}.parts.json");
    let manifest_bytes = fs::read(parts_dir.join(&manifest_name)).unwrap();
    for part in &manifest.parts {
        by_name.insert(part.name.clone(), fs::read(parts_dir.join(&part.name)).unwrap());
    }
    by_name.insert(manifest_name.clone(), manifest_bytes);
    by_name.insert("FNF_0.9.9_Server_Win64.zip".into(), zip_bytes(&[("FridayNightFightsServer.exe", b"srv".to_vec())]));
    let sizes: Vec<(String, usize)> = by_name.iter().map(|(k, v)| (k.clone(), v.len())).collect();
    for (name, size) in &sizes {
        assets.push((name.clone(), *size));
    }
    let corrupt = manifest.parts[1].name.clone();
    let api = serve(
        Files { by_name, corrupt_once: Some(corrupt), hits: HashMap::new() },
        move |base| {
            let list: Vec<String> = assets.iter().map(|(n, s)| asset_json(base, n, *s)).collect();
            format!(
                r#"[{{"tag_name":"v0.9.9","name":"FNF 0.9.9","draft":false,"prerelease":false,"assets":[{}]}},
                    {{"tag_name":"v0.9.8.5","name":"old","draft":false,"prerelease":false,"assets":[{}]}}]"#,
                list.join(","),
                asset_json(base, "FNF_0.9.8.5_Client_Win64.zip", 10)
            )
        },
    );

    let job = job(&api, temp.path());
    fs::create_dir_all(job.install_dir.join("FridayNightFights/Saved/SaveGames")).unwrap();
    fs::write(job.install_dir.join("FridayNightFights/Saved/SaveGames/FNF_Account.sav"), b"keep").unwrap();
    let client = reqwest::Client::new();
    let mut phases = Vec::new();
    let installed = patch_game(&client, &job, |p| phases.push(p.message.clone()))
        .await
        .expect("split patch should install");
    assert_eq!(installed.version, "v0.9.9");
    assert_eq!(installed.asset_name, zip_name);
    assert_eq!(fs::read(job.install_dir.join("FridayNightFights.exe")).unwrap(), b"client-v2");
    assert_eq!(
        fs::read(job.install_dir.join("FridayNightFights/Content/Paks/FridayNightFights-Windows.ucas")).unwrap(),
        pak
    );
    assert_eq!(fs::read(job.install_dir.join("FridayNightFights/Saved/SaveGames/FNF_Account.sav")).unwrap(), b"keep");
    assert!(phases.iter().any(|m| m.starts_with("Downloading part")), "{phases:?}");
    // Parts and the joined zip are cleaned up after install.
    let leftovers: Vec<_> = fs::read_dir(&job.download_dir).unwrap().flatten().map(|e| e.file_name()).collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn split_release_with_a_missing_part_fails_without_touching_the_install() {
    let temp = tempfile::tempdir().unwrap();
    let zip = zip_bytes(&[("FridayNightFights.exe", vec![9u8; 40_000])]);
    let zip_name = "FNF_1.0_Client_Win64.zip";
    let src = temp.path().join(zip_name);
    fs::write(&src, &zip).unwrap();
    let parts_dir = temp.path().join("parts");
    let manifest = split_file(&src, &parts_dir, 8 * 1024).unwrap();
    let mut by_name = HashMap::new();
    let manifest_name = format!("{zip_name}.parts.json");
    by_name.insert(manifest_name.clone(), fs::read(parts_dir.join(&manifest_name)).unwrap());
    // Publish every part except the last one.
    for part in &manifest.parts[..manifest.parts.len() - 1] {
        by_name.insert(part.name.clone(), fs::read(parts_dir.join(&part.name)).unwrap());
    }
    let assets: Vec<(String, usize)> = by_name.iter().map(|(k, v)| (k.clone(), v.len())).collect();
    let api = serve(Files { by_name, corrupt_once: None, hits: HashMap::new() }, move |base| {
        let list: Vec<String> = assets.iter().map(|(n, s)| asset_json(base, n, *s)).collect();
        format!(r#"[{{"tag_name":"v1.0","draft":false,"prerelease":false,"assets":[{}]}}]"#, list.join(","))
    });
    let job = job(&api, temp.path());
    fs::create_dir_all(&job.install_dir).unwrap();
    fs::write(job.install_dir.join("FridayNightFights.exe"), b"old").unwrap();
    let err = patch_game(&reqwest::Client::new(), &job, |_| {}).await.unwrap_err();
    assert!(err.to_string().contains("missing part"), "{err}");
    assert_eq!(fs::read(job.install_dir.join("FridayNightFights.exe")).unwrap(), b"old");
}
