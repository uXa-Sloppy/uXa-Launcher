use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::Serialize;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

use crate::github::{auth_headers, download_url, find_latest_release};
use crate::model::InstalledManifest;
use crate::{Error, ReleaseSource, Result};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchProgress {
    pub phase: String,
    pub received: u64,
    pub total: u64,
    pub percent: u8,
    pub message: String,
}

pub struct DownloadRequest<'a> {
    pub client: &'a reqwest::Client,
    pub url: &'a str,
    pub dest: &'a Path,
    pub expected_size: u64,
    pub token: Option<&'a str>,
}

pub struct PatchJob {
    pub source: ReleaseSource,
    pub install_dir: PathBuf,
    pub download_dir: PathBuf,
    pub preserve: Vec<String>,
    pub game_id: String,
}

pub fn asset_matches(name: &str, contains: &[String], pattern: Option<&regex::Regex>) -> bool {
    let lower = name.to_ascii_lowercase();
    if !lower.ends_with(".zip") {
        return false;
    }
    if !contains
        .iter()
        .filter(|part| !part.is_empty())
        .all(|part| name.contains(part.as_str()))
    {
        return false;
    }
    match pattern {
        Some(pattern) => pattern.is_match(name),
        None => true,
    }
}

fn percent(received: u64, total: u64) -> u8 {
    if total == 0 {
        0
    } else {
        ((received.saturating_mul(100)) / total).min(100) as u8
    }
}

fn file_name_only(name: &str) -> String {
    Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty() && s != "." && s != "..")
        .unwrap_or_else(|| "download.zip".to_string())
}

pub async fn download_with_resume<F>(request: DownloadRequest<'_>, mut progress: F) -> Result<u64>
where
    F: FnMut(PatchProgress),
{
    if let Some(parent) = request.dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut existing = if request.dest.exists() {
        fs::metadata(request.dest)?.len()
    } else {
        0
    };
    if request.expected_size > 0 && existing > request.expected_size {
        tracing::warn!("partial download is larger than the asset; starting over");
        fs::remove_file(request.dest)?;
        existing = 0;
    }
    if request.expected_size > 0 && existing == request.expected_size {
        progress(PatchProgress {
            phase: "download".into(),
            received: existing,
            total: request.expected_size,
            percent: 100,
            message: "Download already complete".into(),
        });
        return Ok(existing);
    }

    let mut headers = auth_headers(request.token, "application/octet-stream")?;
    if existing > 0 {
        headers.insert(
            reqwest::header::RANGE,
            reqwest::header::HeaderValue::from_str(&format!("bytes={existing}-"))
                .map_err(|e| Error::msg(e.to_string()))?,
        );
        tracing::info!("resuming download at byte {existing}");
    }

    let response = request
        .client
        .get(request.url)
        .headers(headers)
        .send()
        .await?;
    let status = response.status();
    if status == StatusCode::RANGE_NOT_SATISFIABLE && request.expected_size > 0 && existing == request.expected_size
    {
        return Ok(existing);
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::msg(format!(
            "Download failed ({status}): {}",
            body.chars().take(160).collect::<String>()
        )));
    }

    let append = status == StatusCode::PARTIAL_CONTENT && existing > 0;
    if !append {
        existing = 0;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!append)
        .append(append)
        .open(request.dest)?;
    if !append {
        file.set_len(0)?;
    }

    let mut received = existing;
    let total = if request.expected_size > 0 {
        request.expected_size
    } else {
        response.content_length().unwrap_or(0).saturating_add(existing)
    };
    let mut stream = response.bytes_stream();
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk)?;
        received += chunk.len() as u64;
        if last_emit.elapsed() >= Duration::from_millis(120) {
            progress(PatchProgress {
                phase: "download".into(),
                received,
                total,
                percent: percent(received, total),
                message: "Downloading".into(),
            });
            last_emit = Instant::now();
        }
    }
    file.sync_all()?;
    progress(PatchProgress {
        phase: "download".into(),
        received,
        total: if total == 0 { received } else { total },
        percent: percent(received, if total == 0 { received } else { total }),
        message: "Download finished".into(),
    });
    Ok(received)
}

pub fn check_zip_magic(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).map_err(|_| Error::msg("download is not a zip archive"))?;
    if &magic != b"PK\x03\x04" && &magic != b"PK\x05\x06" && &magic != b"PK\x07\x08" {
        return Err(Error::msg("download is not a zip archive"));
    }
    Ok(())
}

pub fn verify_download(path: &Path, expected_size: u64, digest: Option<&str>) -> Result<()> {
    let len = fs::metadata(path)?.len();
    if expected_size > 0 && len != expected_size {
        return Err(Error::msg(format!(
            "download is {len} bytes, release asset is {expected_size} bytes"
        )));
    }
    check_zip_magic(path)?;
    if let Some(digest) = digest.map(str::trim).filter(|d| !d.is_empty()) {
        check_digest(path, digest)?;
    }
    // Central directory must parse before we touch the live install.
    let file = File::open(path)?;
    let _archive = ZipArchive::new(file)?;
    Ok(())
}

fn check_digest(path: &Path, digest: &str) -> Result<()> {
    let hex_str = digest.strip_prefix("sha256:").unwrap_or(digest);
    if hex_str.len() != 64 || !hex_str.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::msg(format!("unsupported digest format ({digest})")));
    }
    let expected = hex::decode(hex_str).map_err(|e| Error::msg(format!("bad digest: {e}")))?;
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
    let got = hasher.finalize();
    if got.as_slice() != expected.as_slice() {
        return Err(Error::msg(
            "download checksum does not match the release digest",
        ));
    }
    Ok(())
}

pub fn extract_zip(archive_path: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    let file = File::open(archive_path)?;
    let mut archive = ZipArchive::new(file)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        if name.starts_with("__MACOSX/") || name == "__MACOSX" {
            continue;
        }
        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                return Err(Error::msg(format!("refusing symlink in archive: {name}")));
            }
        }
        let Some(rel) = entry.enclosed_name() else {
            return Err(Error::msg(format!("blocked unsafe zip path: {name}")));
        };
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::msg(format!("blocked unsafe zip path: {name}")));
        }
        let out = dest.join(rel);
        if !out.starts_with(dest) {
            return Err(Error::msg(format!("blocked unsafe zip path: {name}")));
        }
        if entry.is_dir() || name.ends_with('/') {
            fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut outfile = File::create(&out)?;
        std::io::copy(&mut entry, &mut outfile)?;
        outfile.sync_all()?;
    }
    Ok(())
}

pub fn copy_preserved(from_install: &Path, staging: &Path, relatives: &[String]) -> Result<()> {
    for relative in relatives {
        let rel = Path::new(relative.trim());
        if relative.trim().is_empty()
            || rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(Error::msg(format!(
                "refusing to preserve path {relative:?}"
            )));
        }
        let src = from_install.join(rel);
        if !src.exists() {
            continue;
        }
        let dst = staging.join(rel);
        tracing::info!("preserving {} -> {}", src.display(), dst.display());
        copy_tree(&src, &dst)?;
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        tracing::warn!("skipping symlink {}", src.display());
        return Ok(());
    }
    if meta.is_file() {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        return Ok(());
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
    }
    Ok(())
}

fn move_dir(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) => {
            tracing::warn!(
                "rename {} -> {} failed ({err}); copying across the filesystem",
                from.display(),
                to.display()
            );
            copy_tree(from, to)?;
            fs::remove_dir_all(from)?;
            Ok(())
        }
    }
}

pub fn previous_install_dir(install: &Path) -> Result<PathBuf> {
    let parent = install
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::msg("install path has no parent"))?;
    let name = install
        .file_name()
        .ok_or_else(|| Error::msg("install path has no name"))?;
    Ok(parent.join(format!("{}.previous", name.to_string_lossy())))
}

fn failed_install_dir(install: &Path) -> Result<PathBuf> {
    let parent = install
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::msg("install path has no parent"))?;
    let name = install
        .file_name()
        .ok_or_else(|| Error::msg("install path has no name"))?;
    Ok(parent.join(format!("{}.failed", name.to_string_lossy())))
}

/// Replace `install` with `staging`. When `keep_backup` is false the previous
/// copy is removed after a successful swap, which is the client patcher path.
fn swap_dirs(staging: &Path, install: &Path, keep_backup: bool) -> Result<()> {
    if !staging.is_dir() {
        return Err(Error::msg("staging directory is missing"));
    }
    let parent = install
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::msg("install path has no parent"))?;
    fs::create_dir_all(parent)?;
    let backup = previous_install_dir(install)?;
    if backup.exists() {
        fs::remove_dir_all(&backup)?;
    }
    let moved_aside = if install.exists() {
        move_dir(install, &backup)?;
        true
    } else {
        false
    };
    if let Err(err) = move_dir(staging, install) {
        if install.exists() {
            let _ = fs::remove_dir_all(install);
        }
        if moved_aside {
            if let Err(restore_err) = move_dir(&backup, install) {
                return Err(Error::msg(format!(
                    "update failed ({err}) and restoring the previous install also failed ({restore_err}). Previous files are still in {}",
                    backup.display()
                )));
            }
        }
        return Err(err);
    }
    if !keep_backup && backup.exists() {
        if let Err(err) = fs::remove_dir_all(&backup) {
            tracing::warn!(
                "updated {} but could not remove the previous copy at {}: {err}",
                install.display(),
                backup.display()
            );
        }
    }
    Ok(())
}

/// Replace `install` with `staging`. On failure the previous install is put back.
pub fn atomic_swap(staging: &Path, install: &Path) -> Result<()> {
    swap_dirs(staging, install, false)
}

/// Same swap as [`atomic_swap`], but the previous install stays at
/// `{name}.previous` so a failed health check can restore it.
pub fn swap_keeping_previous(staging: &Path, install: &Path) -> Result<()> {
    swap_dirs(staging, install, true)
}

/// Put `{name}.previous` back at `install`. The rejected install is moved to `{name}.failed`.
pub fn restore_previous(install: &Path) -> Result<()> {
    let backup = previous_install_dir(install)?;
    if !backup.is_dir() {
        return Err(Error::msg(format!(
            "no previous install to restore at {}",
            backup.display()
        )));
    }
    if install.exists() {
        let failed = failed_install_dir(install)?;
        if failed.exists() {
            fs::remove_dir_all(&failed)?;
        }
        move_dir(install, &failed)?;
    }
    move_dir(&backup, install)?;
    Ok(())
}

pub fn discard_previous(install: &Path) -> Result<()> {
    let backup = previous_install_dir(install)?;
    if backup.exists() {
        fs::remove_dir_all(&backup)?;
    }
    Ok(())
}

pub async fn patch_game<F>(
    client: &reqwest::Client,
    job: &PatchJob,
    mut progress: F,
) -> Result<InstalledManifest>
where
    F: FnMut(PatchProgress),
{
    progress(PatchProgress {
        phase: "check".into(),
        received: 0,
        total: 0,
        percent: 0,
        message: "Looking up the latest release".into(),
    });
    let release = find_latest_release(client, &job.source).await?;
    let url = download_url(&release.asset)?.to_string();
    fs::create_dir_all(&job.download_dir)?;
    let dest = job
        .download_dir
        .join(format!("{}.partial", file_name_only(&release.asset.name)));

    let received = download_with_resume(
        DownloadRequest {
            client,
            url: &url,
            dest: &dest,
            expected_size: release.asset.size,
            token: job.source.token.as_deref(),
        },
        &mut progress,
    )
    .await?;
    let _ = received;

    progress(PatchProgress {
        phase: "verify".into(),
        received: release.asset.size,
        total: release.asset.size,
        percent: 100,
        message: "Verifying archive".into(),
    });
    if let Err(err) = verify_download(&dest, release.asset.size, release.asset.digest.as_deref()) {
        let incomplete = fs::metadata(&dest)
            .map(|meta| release.asset.size > 0 && meta.len() < release.asset.size)
            .unwrap_or(false);
        if !incomplete {
            let _ = fs::remove_file(&dest);
        }
        return Err(err);
    }

    let parent = job
        .install_dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::msg("install path has no parent"))?;
    fs::create_dir_all(parent)?;
    let staging_name = format!(
        ".{}.staging",
        job.install_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| job.game_id.clone())
    );
    let staging = parent.join(staging_name);
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }

    progress(PatchProgress {
        phase: "extract".into(),
        received: 0,
        total: 0,
        percent: 100,
        message: "Extracting".into(),
    });
    if let Err(err) = extract_zip(&dest, &staging) {
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_file(&dest);
        return Err(err);
    }

    if job.install_dir.exists() {
        progress(PatchProgress {
            phase: "preserve".into(),
            received: 0,
            total: 0,
            percent: 100,
            message: "Keeping saves and config".into(),
        });
        if let Err(err) = copy_preserved(&job.install_dir, &staging, &job.preserve) {
            let _ = fs::remove_dir_all(&staging);
            return Err(err);
        }
    }

    progress(PatchProgress {
        phase: "swap".into(),
        received: 0,
        total: 0,
        percent: 100,
        message: "Installing".into(),
    });
    if let Err(err) = atomic_swap(&staging, &job.install_dir) {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }
    let _ = fs::remove_file(&dest);

    let manifest = InstalledManifest {
        game_id: job.game_id.clone(),
        version: release.tag.clone(),
        asset_name: release.asset.name.clone(),
        install_path: job.install_dir.display().to_string(),
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    progress(PatchProgress {
        phase: "done".into(),
        received: release.asset.size,
        total: release.asset.size,
        percent: 100,
        message: format!("Installed {}", release.tag),
    });
    Ok(manifest)
}
