//! Download and stage a newer uXa Launcher executable.
//!
//! The file swap renames the running exe to `uxa-launcher.exe.old` and copies
//! the verified download into its place. Windows allows that rename while the
//! process is still running. If the copy fails, the old file is renamed back
//! and the process keeps running the previous version.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::github::{
    download_url, fetch_latest_release, repo_visibility, FullRelease, RepoVisibility,
};
use crate::patch::{download_with_resume, DownloadRequest, PatchProgress};
use crate::version::is_newer;
use crate::{Error, ReleaseAsset, Result};

pub const DEFAULT_UPDATE_OWNER: &str = "uXa-Sloppy";
pub const DEFAULT_UPDATE_REPO: &str = "uXa-Launcher";
pub const LAUNCHER_EXE_ASSET: &str = "uxa-launcher.exe";
pub const CHECKSUM_ASSET: &str = "SHA256SUMS.txt";
pub const INSTALL_DIR_NAME: &str = "uXa Launcher";

pub fn resolve_update_repo(
    settings_owner: &str,
    settings_repo: &str,
    catalog_owner: &str,
    catalog_repo: &str,
) -> (String, String) {
    (
        pick(settings_owner, catalog_owner, DEFAULT_UPDATE_OWNER),
        pick(settings_repo, catalog_repo, DEFAULT_UPDATE_REPO),
    )
}

fn pick(primary: &str, fallback: &str, default: &str) -> String {
    let primary = primary.trim();
    if !primary.is_empty() {
        return primary.to_string();
    }
    let fallback = fallback.trim();
    if !fallback.is_empty() {
        return fallback.to_string();
    }
    default.to_string()
}

/// Saved token is attached only when the repository is private.
pub fn token_for_visibility(
    visibility: RepoVisibility,
    saved: Option<&str>,
) -> Result<Option<String>> {
    let saved = saved.map(str::trim).filter(|token| !token.is_empty());
    match visibility {
        RepoVisibility::Public | RepoVisibility::NotVisible => Ok(None),
        RepoVisibility::Private => match saved {
            Some(token) => Ok(Some(token.to_string())),
            None => Err(Error::msg(
                "The launcher update repository is private. Add a GitHub token in Settings to install the update. The installed launcher was not changed.",
            )),
        },
    }
}

pub fn redact_secrets(text: &str) -> String {
    let mut out = text.to_string();
    for pattern in secret_patterns() {
        out = pattern.replace_all(&out, "[redacted]").into_owned();
    }
    out
}

fn secret_patterns() -> &'static [regex::Regex] {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            "github_pat_[A-Za-z0-9_]+",
            "ghp_[A-Za-z0-9]+",
            "gho_[A-Za-z0-9]+",
            r"(?i)bearer\s+\S+",
        ]
        .into_iter()
        .map(|pattern| regex::Regex::new(pattern).expect("secret pattern"))
        .collect()
    })
}

pub fn select_launcher_exe(assets: &[ReleaseAsset]) -> Result<&ReleaseAsset> {
    assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(LAUNCHER_EXE_ASSET))
        .ok_or_else(|| {
            Error::msg(
                "This release has no uxa-launcher.exe asset. The installed launcher was not changed.",
            )
        })
}

pub fn select_checksum<'a>(assets: &'a [ReleaseAsset]) -> Option<&'a ReleaseAsset> {
    assets
        .iter()
        .find(|asset| asset.name.eq_ignore_ascii_case(CHECKSUM_ASSET))
}

/// `sha256sum` lines: `<64 hex>  <filename>`, with an optional `*` before the name.
pub fn parse_sha256sums(text: &str, file_name: &str) -> Result<String> {
    let mut found = None;
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((hash, name)) = split_checksum_line(line) else {
            return Err(Error::msg(format!(
                "SHA256SUMS.txt line {} is not 'hash  filename'. The installed launcher was not changed.",
                index + 1
            )));
        };
        let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
        if base.eq_ignore_ascii_case(file_name) {
            if found.is_some() {
                return Err(Error::msg(format!(
                    "SHA256SUMS.txt lists {file_name} more than once. The installed launcher was not changed."
                )));
            }
            if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(Error::msg(
                    "SHA256SUMS.txt has a checksum that is not 64 hex characters. The installed launcher was not changed.",
                ));
            }
            found = Some(hash.to_ascii_lowercase());
        }
    }
    found.ok_or_else(|| {
        Error::msg(format!(
            "SHA256SUMS.txt has no entry for {file_name}. The installed launcher was not changed."
        ))
    })
}

fn split_checksum_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split_whitespace();
    let hash = parts.next()?;
    let rest = line[hash.len()..].trim();
    if rest.is_empty() {
        return None;
    }
    let name = rest.strip_prefix('*').unwrap_or(rest).trim();
    if name.is_empty() {
        None
    } else {
        Some((hash, name))
    }
}

pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn verify_exe_file(path: &Path, expected_size: u64, sha256_hex: Option<&str>) -> Result<()> {
    let len = fs::metadata(path)?.len();
    if expected_size == 0 || len != expected_size {
        return Err(Error::msg(format!(
            "The download is {len} bytes, but the release asset is {expected_size} bytes. The installed launcher was not changed."
        )));
    }
    let mut file = File::open(path)?;
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).map_err(|_| {
        Error::msg(
            "The download is not a Windows executable. The installed launcher was not changed.",
        )
    })?;
    if &magic != b"MZ" {
        return Err(Error::msg(
            "The download is not a Windows executable. The installed launcher was not changed.",
        ));
    }
    if let Some(expected) = sha256_hex.map(str::trim).filter(|value| !value.is_empty()) {
        let expected = expected
            .strip_prefix("sha256:")
            .unwrap_or(expected)
            .to_ascii_lowercase();
        let got = file_sha256(path)?;
        if got != expected {
            return Err(Error::msg(
                "The download checksum does not match the release. The installed launcher was not changed.",
            ));
        }
    }
    Ok(())
}

pub fn backup_path(current: &Path) -> PathBuf {
    let name = current
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| LAUNCHER_EXE_ASSET.to_string());
    current.with_file_name(format!("{name}.old"))
}

pub fn installed_launcher_path(local_app_data: &Path) -> PathBuf {
    local_app_data
        .join(INSTALL_DIR_NAME)
        .join(LAUNCHER_EXE_ASSET)
}

pub fn running_from_cargo_target(path: &Path) -> bool {
    let text = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    text.contains("\\target\\debug\\") || text.contains("\\target\\release\\")
}

/// Delete `uxa-launcher.exe.old` left by a previous successful swap.
pub fn cleanup_previous_launcher(current: &Path) {
    let backup = backup_path(current);
    if !backup.is_file() {
        return;
    }
    match fs::remove_file(&backup) {
        Ok(()) => tracing::info!(path = %backup.display(), "removed previous launcher backup"),
        Err(err) => {
            tracing::warn!(path = %backup.display(), "could not remove previous launcher backup: {err}")
        }
    }
}

pub fn replace_executable(current: &Path, new_file: &Path) -> Result<()> {
    replace_executable_with(current, new_file, copy_and_sync)
}

pub fn replace_executable_with(
    current: &Path,
    new_file: &Path,
    copy_file: impl Fn(&Path, &Path) -> std::io::Result<u64>,
) -> Result<()> {
    if !new_file.is_file() {
        return Err(Error::msg(
            "The downloaded launcher file is missing. The installed launcher was not changed.",
        ));
    }
    if !current.is_file() {
        return Err(Error::msg(
            "The running launcher executable could not be found. The installed launcher was not changed.",
        ));
    }
    let backup = backup_path(current);
    if backup.exists() {
        fs::remove_file(&backup).map_err(|err| {
            Error::msg(format!(
                "Could not remove the previous backup {}. The installed launcher was not changed. {err}",
                backup.display()
            ))
        })?;
    }
    fs::rename(current, &backup).map_err(|err| {
        Error::msg(format!(
            "Could not move the running launcher aside. The installed launcher was not changed. {err}"
        ))
    })?;
    match copy_file(new_file, current) {
        Ok(copied) if copied > 0 && current.is_file() => Ok(()),
        Ok(_) => {
            let _ = restore_previous_launcher(current);
            Err(Error::msg(
                "The new launcher file was empty. The previous version was restored.",
            ))
        }
        Err(err) => {
            if let Err(restore_err) = restore_previous_launcher(current) {
                return Err(Error::msg(format!(
                    "Could not install the new launcher ({err}), and could not restore the previous one ({restore_err})."
                )));
            }
            Err(Error::msg(format!(
                "Could not install the new launcher ({err}). The previous version was restored."
            )))
        }
    }
}

fn copy_and_sync(src: &Path, dest: &Path) -> std::io::Result<u64> {
    let copied = fs::copy(src, dest)?;
    if let Ok(file) = OpenOptions::new().write(true).open(dest) {
        let _ = file.sync_all();
    }
    Ok(copied)
}

pub fn restore_previous_launcher(current: &Path) -> Result<()> {
    let backup = backup_path(current);
    if !backup.is_file() {
        return Err(Error::msg("no previous launcher backup"));
    }
    if current.exists() {
        fs::remove_file(current)?;
    }
    fs::rename(&backup, current)?;
    Ok(())
}

#[derive(Debug)]
pub struct PreparedLauncherUpdate {
    pub latest: String,
    pub url: String,
    pub notes: String,
    pub exe_path: PathBuf,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Debug)]
pub enum LauncherUpdateFetch {
    UpToDate,
    /// Could not reach GitHub, or the repo is not visible. The running exe stays as it is.
    Skipped(String),
    Ready(PreparedLauncherUpdate),
}

pub async fn download_launcher_update<F>(
    client: &reqwest::Client,
    api_base: &str,
    owner: &str,
    repo: &str,
    current_version: &str,
    saved_token: Option<&str>,
    download_dir: &Path,
    mut progress: F,
) -> Result<LauncherUpdateFetch>
where
    F: FnMut(PatchProgress),
{
    let owner = owner.trim();
    let repo = repo.trim();
    tracing::info!(%owner, %repo, "checking launcher update repository");
    let visibility = match repo_visibility(client, api_base, owner, repo, saved_token).await {
        Ok(visibility) => visibility,
        Err(err) => {
            let message = redact_secrets(&err.to_string());
            tracing::warn!("launcher update check skipped: {message}");
            return Ok(LauncherUpdateFetch::Skipped(message));
        }
    };
    let private = matches!(visibility, RepoVisibility::Private);
    tracing::info!(%owner, %repo, private, "launcher repository visibility");
    let token = token_for_visibility(visibility, saved_token)?;

    let release = match fetch_latest_release(client, api_base, owner, repo, token.as_deref()).await
    {
        Ok(release) => release,
        Err(err) => {
            let message = redact_secrets(&err.to_string());
            if private {
                return Err(unchanged(message));
            }
            tracing::warn!("launcher update check skipped: {message}");
            return Ok(LauncherUpdateFetch::Skipped(message));
        }
    };
    let Some(release) = release else {
        tracing::info!("no launcher release to install");
        return Ok(LauncherUpdateFetch::UpToDate);
    };
    if release.draft || release.prerelease {
        tracing::info!(tag = %release.tag, "ignoring draft or prerelease launcher update");
        return Ok(LauncherUpdateFetch::UpToDate);
    }
    if !is_newer(&release.tag, Some(current_version))? {
        tracing::info!(tag = %release.tag, current = %current_version, "launcher is current");
        return Ok(LauncherUpdateFetch::UpToDate);
    }
    stage_release(
        client,
        token.as_deref(),
        &release,
        download_dir,
        &mut progress,
    )
    .await
    .map_err(|err| Error::msg(redact_secrets(&err.to_string())))
}

async fn stage_release<F>(
    client: &reqwest::Client,
    token: Option<&str>,
    release: &FullRelease,
    download_dir: &Path,
    progress: &mut F,
) -> Result<LauncherUpdateFetch>
where
    F: FnMut(PatchProgress),
{
    let exe = select_launcher_exe(&release.assets)?;
    if exe.size == 0 {
        return Err(Error::msg(
            "The launcher asset has no size. The installed launcher was not changed.",
        ));
    }
    fs::create_dir_all(download_dir)?;
    let exe_path = download_dir.join("uxa-launcher.exe.download");
    if exe_path.exists() {
        let _ = fs::remove_file(&exe_path);
    }
    progress(PatchProgress {
        phase: "downloading".into(),
        received: 0,
        total: exe.size,
        percent: 0,
        message: format!("Downloading launcher {}", release.tag),
    });
    download_with_resume(
        DownloadRequest {
            client,
            url: download_url(exe)?,
            dest: &exe_path,
            expected_size: exe.size,
            token,
        },
        |event| {
            progress(PatchProgress {
                message: format!("Downloading launcher {}", release.tag),
                ..event
            });
        },
    )
    .await
    .map_err(|err| unchanged(err.to_string()))?;

    progress(PatchProgress {
        phase: "verifying".into(),
        received: exe.size,
        total: exe.size,
        percent: 100,
        message: format!("Verifying launcher {}", release.tag),
    });

    let sha256 = match select_checksum(&release.assets) {
        Some(checksum) => {
            if checksum.size == 0 {
                let _ = fs::remove_file(&exe_path);
                return Err(Error::msg(
                    "SHA256SUMS.txt has no size. The installed launcher was not changed.",
                ));
            }
            let sums_path = download_dir.join(CHECKSUM_ASSET);
            download_with_resume(
                DownloadRequest {
                    client,
                    url: download_url(checksum)?,
                    dest: &sums_path,
                    expected_size: checksum.size,
                    token,
                },
                |_| {},
            )
            .await
            .map_err(|err| unchanged(err.to_string()))?;
            let sums_len = fs::metadata(&sums_path)?.len();
            if sums_len != checksum.size {
                let _ = fs::remove_file(&exe_path);
                let _ = fs::remove_file(&sums_path);
                return Err(Error::msg(format!(
                    "SHA256SUMS.txt is {sums_len} bytes, but the release asset is {} bytes. The installed launcher was not changed.",
                    checksum.size
                )));
            }
            let text = fs::read_to_string(&sums_path)
                .map_err(|err| unchanged(format!("Could not read SHA256SUMS.txt ({err})")))?;
            let hex_sum = parse_sha256sums(&text, &exe.name)?;
            if let Some(digest) = exe.digest.as_deref() {
                let from_api = digest
                    .trim()
                    .strip_prefix("sha256:")
                    .unwrap_or(digest.trim())
                    .to_ascii_lowercase();
                if from_api != hex_sum {
                    let _ = fs::remove_file(&exe_path);
                    return Err(Error::msg(
                        "The GitHub asset digest and SHA256SUMS.txt disagree. The installed launcher was not changed.",
                    ));
                }
            }
            Some(hex_sum)
        }
        None => exe.digest.as_ref().map(|digest| {
            digest
                .trim()
                .strip_prefix("sha256:")
                .unwrap_or(digest.trim())
                .to_ascii_lowercase()
        }),
    };

    if let Err(err) = verify_exe_file(&exe_path, exe.size, sha256.as_deref()) {
        let _ = fs::remove_file(&exe_path);
        return Err(err);
    }

    Ok(LauncherUpdateFetch::Ready(PreparedLauncherUpdate {
        latest: release.tag.clone(),
        url: release.html_url.clone(),
        notes: release.body.clone(),
        exe_path,
        size: exe.size,
        sha256,
    }))
}

fn unchanged(message: impl AsRef<str>) -> Error {
    let message = redact_secrets(message.as_ref());
    if message.contains("installed launcher was not changed")
        || message.contains("previous version was restored")
    {
        Error::msg(message)
    } else {
        Error::msg(format!("{message} The installed launcher was not changed."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ReleaseAsset;

    fn asset(name: &str, size: u64) -> ReleaseAsset {
        ReleaseAsset {
            name: name.into(),
            api_url: format!("https://example.test/{name}"),
            browser_download_url: String::new(),
            size,
            digest: None,
        }
    }

    #[test]
    fn defaults_and_overrides() {
        assert_eq!(
            resolve_update_repo("", "", "", ""),
            ("uXa-Sloppy".into(), "uXa-Launcher".into())
        );
        assert_eq!(
            resolve_update_repo(" Other ", " Fork ", "uXa-Sloppy", "uXa-Launcher"),
            ("Other".into(), "Fork".into())
        );
        assert_eq!(
            resolve_update_repo("", "", "Guild", "Launcher"),
            ("Guild".into(), "Launcher".into())
        );
    }

    #[test]
    fn token_only_for_private_repos() {
        assert_eq!(
            token_for_visibility(RepoVisibility::Public, Some("ghp_secret")).unwrap(),
            None
        );
        assert_eq!(
            token_for_visibility(RepoVisibility::NotVisible, Some("ghp_secret")).unwrap(),
            None
        );
        assert_eq!(
            token_for_visibility(RepoVisibility::Private, Some(" ghp_secret "))
                .unwrap()
                .as_deref(),
            Some("ghp_secret")
        );
        let err = token_for_visibility(RepoVisibility::Private, None).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("private"));
        assert!(text.contains("not changed"));
        assert!(!text.contains("ghp_"));
    }

    #[test]
    fn redacts_tokens() {
        let text = redact_secrets("auth Bearer ghp_abc12345 and github_pat_zzzz failed");
        assert!(!text.contains("ghp_abc"));
        assert!(!text.contains("github_pat_zzzz"));
        assert!(text.contains("[redacted]"));
    }

    #[test]
    fn selects_the_launcher_exe_only() {
        let assets = vec![
            asset("uXa Launcher_0.3.0_x64-setup.exe", 10),
            asset("uxa-server-agent.exe", 10),
            asset("SHA256SUMS.txt", 10),
            asset("uxa-launcher.exe", 42),
        ];
        assert_eq!(
            select_launcher_exe(&assets).unwrap().name,
            "uxa-launcher.exe"
        );
        assert_eq!(select_checksum(&assets).unwrap().name, "SHA256SUMS.txt");
        assert!(select_launcher_exe(&[asset("setup.exe", 1)]).is_err());
    }

    #[test]
    fn parses_checksum_lines() {
        let text = "\
# comment
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  uXa Launcher_0.3.0_x64-setup.exe
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  uxa-launcher.exe
";
        assert_eq!(
            parse_sha256sums(text, "uxa-launcher.exe").unwrap(),
            "b".repeat(64)
        );
        let starred = format!("{} *uxa-launcher.exe\n", "c".repeat(64));
        assert_eq!(
            parse_sha256sums(&starred, "UXA-LAUNCHER.EXE").unwrap(),
            "c".repeat(64)
        );
        assert!(parse_sha256sums(text, "missing.exe").is_err());
    }

    #[test]
    fn verifies_size_magic_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uxa-launcher.exe");
        fs::write(&path, b"MZ-launcher").unwrap();
        let hash = file_sha256(&path).unwrap();
        verify_exe_file(&path, 11, Some(&hash)).unwrap();
        assert!(verify_exe_file(&path, 4, Some(&hash)).is_err());
        assert!(verify_exe_file(&path, 11, Some(&"d".repeat(64))).is_err());
        fs::write(&path, b"nope-not-exe").unwrap();
        assert!(verify_exe_file(&path, 12, None).is_err());
    }

    #[test]
    fn swap_keeps_neighboring_settings_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("uXa").join("Launcher");
        let install = dir.path().join("uXa Launcher");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&install).unwrap();
        fs::write(data.join("settings.json"), b"{\"checkOnStart\":true}").unwrap();
        fs::write(data.join("servers.json"), b"{\"servers\":true}").unwrap();
        let current = install.join("uxa-launcher.exe");
        fs::write(&current, b"old-bytes").unwrap();
        let staged = dir.path().join("staged.exe");
        fs::write(&staged, b"new-bytes").unwrap();

        replace_executable(&current, &staged).unwrap();
        assert_eq!(fs::read(&current).unwrap(), b"new-bytes");
        assert_eq!(fs::read(backup_path(&current)).unwrap(), b"old-bytes");
        assert_eq!(
            fs::read(data.join("settings.json")).unwrap(),
            b"{\"checkOnStart\":true}"
        );
        assert_eq!(
            fs::read(data.join("servers.json")).unwrap(),
            b"{\"servers\":true}"
        );

        cleanup_previous_launcher(&current);
        assert!(!backup_path(&current).exists());
        assert_eq!(fs::read(&current).unwrap(), b"new-bytes");

        fs::write(&current, b"old-bytes").unwrap();
        let err = replace_executable_with(&current, &staged, |_src, _dest| {
            Err(std::io::Error::other("disk full"))
        })
        .unwrap_err();
        assert!(err.to_string().contains("restored"));
        assert_eq!(fs::read(&current).unwrap(), b"old-bytes");
        assert!(!backup_path(&current).exists());
        assert_eq!(
            fs::read(data.join("servers.json")).unwrap(),
            b"{\"servers\":true}"
        );
    }

    #[test]
    fn missing_download_does_not_touch_the_exe() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("uxa-launcher.exe");
        fs::write(&current, b"stay").unwrap();
        let err = replace_executable(&current, &dir.path().join("missing.exe")).unwrap_err();
        assert!(err.to_string().contains("not changed"));
        assert_eq!(fs::read(&current).unwrap(), b"stay");
    }

    #[test]
    fn cargo_target_paths_are_recognized() {
        assert!(running_from_cargo_target(Path::new(
            r"D:\src\uXa-Launcher\target\debug\uxa-launcher.exe"
        )));
        assert!(running_from_cargo_target(Path::new(
            "/workspace/target/release/uxa-launcher.exe"
        )));
        assert!(!running_from_cargo_target(Path::new(
            r"C:\Users\me\AppData\Local\uXa Launcher\uxa-launcher.exe"
        )));
    }

    #[tokio::test]
    async fn downloads_public_release_without_sending_the_token() {
        let bytes = b"MZ-public-launcher";
        let dir = tempfile::tempdir().unwrap();
        let sample = dir.path().join("sample");
        fs::write(&sample, bytes).unwrap();
        let hash = file_sha256(&sample).unwrap();
        let hash_for_server = hash.clone();
        let sums = format!("{hash}  uxa-launcher.exe\n");
        let saw_auth = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&saw_auth);
        let body_exe = bytes.to_vec();
        let body_sums = sums.clone();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let url = request.url().to_string();
                let authed = request
                    .headers()
                    .iter()
                    .any(|header| header.field.equiv("Authorization"));
                if authed {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                let (status, body, json) = if url.contains("/releases/latest") {
                    let payload = serde_json::json!({
                        "tag_name": "v0.9.0",
                        "name": "v0.9.0",
                        "body": "notes",
                        "draft": false,
                        "prerelease": false,
                        "html_url": format!("http://127.0.0.1:{port}/release"),
                        "assets": [
                            {
                                "name": "uxa-launcher.exe",
                                "url": format!("http://127.0.0.1:{port}/uxa-launcher.exe"),
                                "browser_download_url": format!("http://127.0.0.1:{port}/uxa-launcher.exe"),
                                "size": body_exe.len(),
                                "digest": format!("sha256:{hash_for_server}")
                            },
                            {
                                "name": "uXa Launcher_0.9.0_x64-setup.exe",
                                "url": format!("http://127.0.0.1:{port}/setup.exe"),
                                "browser_download_url": "",
                                "size": 4
                            },
                            {
                                "name": "SHA256SUMS.txt",
                                "url": format!("http://127.0.0.1:{port}/SHA256SUMS.txt"),
                                "browser_download_url": "",
                                "size": body_sums.len()
                            }
                        ]
                    });
                    (200, payload.to_string().into_bytes(), true)
                } else if url.ends_with("/uXa-Launcher") {
                    (200, br#"{"private":false}"#.to_vec(), true)
                } else if url.ends_with("/uxa-launcher.exe") {
                    (200, body_exe.clone(), false)
                } else if url.ends_with("/SHA256SUMS.txt") {
                    (200, body_sums.clone().into_bytes(), false)
                } else {
                    (404, br#"{"message":"no"}"#.to_vec(), true)
                };
                let mut response = tiny_http::Response::from_data(body).with_status_code(status);
                if json {
                    let header =
                        tiny_http::Header::from_bytes(b"Content-Type", b"application/json")
                            .unwrap();
                    response = response.with_header(header);
                }
                let _ = request.respond(response);
            }
        });

        let client = crate::http_client().unwrap();
        let dest = dir.path().join("downloads");
        let mut messages = Vec::new();
        let fetched = download_launcher_update(
            &client,
            &format!("http://127.0.0.1:{port}"),
            "uXa-Sloppy",
            "uXa-Launcher",
            "0.3.0",
            Some("ghp_should_not_be_sent"),
            &dest,
            |event| messages.push(event.message),
        )
        .await
        .unwrap();
        assert!(!saw_auth.load(std::sync::atomic::Ordering::SeqCst));
        match fetched {
            LauncherUpdateFetch::Ready(prepared) => {
                assert_eq!(prepared.latest, "v0.9.0");
                assert_eq!(prepared.sha256.as_deref(), Some(hash.as_str()));
                assert_eq!(fs::read(&prepared.exe_path).unwrap(), bytes);
            }
            other => panic!("expected a ready update, got {other:?}"),
        }
        assert!(messages
            .iter()
            .any(|message| message.contains("Downloading")));
    }

    #[tokio::test]
    async fn private_repo_uses_the_saved_token_and_rejects_a_bad_checksum() {
        let bytes = b"MZ-private-launcher";
        let good = {
            let dir = tempfile::tempdir().unwrap();
            let sample = dir.path().join("sample");
            fs::write(&sample, bytes).unwrap();
            file_sha256(&sample).unwrap()
        };
        let bad_sums = format!("{}  uxa-launcher.exe\n", "a".repeat(64));
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let token_ok = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let token_flag = std::sync::Arc::clone(&token_ok);
        let body = bytes.to_vec();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let url = request.url().to_string();
                let authed = request.headers().iter().any(|header| {
                    header.field.equiv("Authorization")
                        && header.value.as_str() == "Bearer test-token"
                });
                let (status, payload) =
                    if url.contains("/releases/latest") || url.ends_with("/uXa-Launcher") {
                        if !authed {
                            (404, br#"{"message":"Not Found"}"#.to_vec())
                        } else if url.contains("/releases/latest") {
                            token_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                            let json = serde_json::json!({
                                "tag_name": "v0.4.0",
                                "name": "v0.4.0",
                                "body": "",
                                "draft": false,
                                "prerelease": false,
                                "html_url": "",
                                "assets": [
                                    {
                                        "name": "uxa-launcher.exe",
                                        "url": format!("http://127.0.0.1:{port}/uxa-launcher.exe"),
                                        "browser_download_url": "",
                                        "size": body.len(),
                                        "digest": format!("sha256:{good}")
                                    },
                                    {
                                        "name": "SHA256SUMS.txt",
                                        "url": format!("http://127.0.0.1:{port}/SHA256SUMS.txt"),
                                        "browser_download_url": "",
                                        "size": bad_sums.len()
                                    }
                                ]
                            });
                            (200, json.to_string().into_bytes())
                        } else {
                            (200, br#"{"private":true}"#.to_vec())
                        }
                    } else if url.ends_with("/uxa-launcher.exe") {
                        if !authed {
                            (404, br#"{"message":"Not Found"}"#.to_vec())
                        } else {
                            (200, body.clone())
                        }
                    } else if url.ends_with("/SHA256SUMS.txt") {
                        (200, bad_sums.as_bytes().to_vec())
                    } else {
                        (404, br#"{"message":"no"}"#.to_vec())
                    };
                let _ = request
                    .respond(tiny_http::Response::from_data(payload).with_status_code(status));
            }
        });

        let client = crate::http_client().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let err = download_launcher_update(
            &client,
            &format!("http://127.0.0.1:{port}"),
            "uXa-Sloppy",
            "uXa-Launcher",
            "0.3.0",
            Some("test-token"),
            dest.path(),
            |_| {},
        )
        .await
        .unwrap_err();
        let text = err.to_string();
        assert!(token_ok.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            text.contains("checksum") || text.contains("disagree") || text.contains("not changed")
        );
        assert!(!text.contains("test-token"));
        let installed = dest.path().join("uxa-launcher.exe");
        assert!(!installed.exists());
    }

    #[tokio::test]
    async fn private_repo_without_a_token_does_not_download() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let url = request.url().to_string();
                let body: Vec<u8> = if url.ends_with("/uXa-Launcher") {
                    br#"{"private":true}"#.to_vec()
                } else {
                    br#"{"message":"no"}"#.to_vec()
                };
                let _ = request.respond(tiny_http::Response::from_data(body).with_status_code(200));
            }
        });
        let client = crate::http_client().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let err = download_launcher_update(
            &client,
            &format!("http://127.0.0.1:{port}"),
            "uXa-Sloppy",
            "uXa-Launcher",
            "0.3.0",
            None,
            dest.path(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("private"));
        assert!(err.to_string().contains("not changed"));
    }
}
