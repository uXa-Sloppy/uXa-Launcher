//! Release assets larger than GitHub's 2 GB per-file limit.
//!
//! A big zip `X.zip` is published as numbered parts `X.zip.001`, `X.zip.002`, ...
//! plus `X.zip.parts.json`, which lists every part's size and SHA-256 and the
//! size and SHA-256 of the whole file. The launcher (client patch) and the server
//! agent (server deploy) download every part with resume, check each hash
//! (retrying a bad part), join them, check the full hash, then extract as usual.
//! A release that ships a single `X.zip` is handled exactly as before.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::github::{download_url, ReleaseAsset};
use crate::patch::{download_with_resume, verify_download, DownloadRequest, PatchProgress};
use crate::{Error, Result};

/// Suffix of the manifest asset that sits next to the parts: `X.zip.parts.json`.
pub const PARTS_MANIFEST_SUFFIX: &str = ".parts.json";
/// Default part size used by the publish script: 1.9 GiB, under GitHub's 2 GiB limit.
pub const DEFAULT_PART_SIZE: u64 = 1_900 * 1024 * 1024;
/// GitHub refuses release assets of 2 GiB or more.
pub const GITHUB_ASSET_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const PART_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartEntry {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartsManifest {
    #[serde(default = "one")]
    pub version: u32,
    /// Name of the joined file, e.g. `FNF_0.9.9_Client_Win64.zip`.
    pub file: String,
    pub size: u64,
    pub sha256: String,
    pub parts: Vec<PartEntry>,
}

fn one() -> u32 {
    1
}

/// The manifest asset plus the part assets, in part order.
#[derive(Debug, Clone)]
pub struct SplitAsset {
    pub manifest: ReleaseAsset,
    pub parts: Vec<ReleaseAsset>,
}

/// `X.zip.001` -> Some(("X.zip", 1)).
pub fn part_number(name: &str) -> Option<(&str, u32)> {
    let (base, num) = name.rsplit_once('.')?;
    if num.len() < 3 || !num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((base, num.parse().ok()?))
}

/// When a release has no single zip for `matches`, look for `X.zip.parts.json`
/// with numbered parts and return a virtual asset named `X.zip`.
pub fn find_split_asset<F>(assets: &[ReleaseAsset], matches: F) -> Option<ReleaseAsset>
where
    F: Fn(&str) -> bool,
{
    for manifest in assets {
        let Some(base) = manifest.name.strip_suffix(PARTS_MANIFEST_SUFFIX) else {
            continue;
        };
        if !matches(base) {
            continue;
        }
        let mut parts: Vec<(u32, ReleaseAsset)> = assets
            .iter()
            .filter_map(|asset| match part_number(&asset.name) {
                Some((b, n)) if b == base => Some((n, asset.clone())),
                _ => None,
            })
            .collect();
        if parts.is_empty() {
            continue;
        }
        parts.sort_by_key(|(n, _)| *n);
        let parts: Vec<ReleaseAsset> = parts.into_iter().map(|(_, a)| a).collect();
        let size = parts.iter().map(|p| p.size).sum();
        return Some(ReleaseAsset {
            name: base.to_string(),
            api_url: manifest.api_url.clone(),
            browser_download_url: manifest.browser_download_url.clone(),
            size,
            digest: None,
            split: Some(Box::new(SplitAsset {
                manifest: manifest.clone(),
                parts,
            })),
        });
    }
    None
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn clean_hex(text: &str) -> String {
    text.trim()
        .strip_prefix("sha256:")
        .unwrap_or(text.trim())
        .to_ascii_lowercase()
}

/// Check the manifest against the part assets the release actually has.
pub fn validate_manifest(manifest: &PartsManifest, split: &SplitAsset) -> Result<()> {
    if manifest.parts.is_empty() {
        return Err(Error::msg("parts manifest lists no parts"));
    }
    let total: u64 = manifest.parts.iter().map(|p| p.size).sum();
    if total != manifest.size {
        return Err(Error::msg(format!(
            "parts manifest sizes add up to {total} bytes, but the file is {} bytes",
            manifest.size
        )));
    }
    let full = clean_hex(&manifest.sha256);
    if full.len() != 64 || !full.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::msg("parts manifest has no valid sha256 for the whole file"));
    }
    for part in &manifest.parts {
        let hex = clean_hex(&part.sha256);
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::msg(format!("part {} has no valid sha256", part.name)));
        }
        let asset = split
            .parts
            .iter()
            .find(|a| a.name == part.name)
            .ok_or_else(|| Error::msg(format!("release is missing part {}", part.name)))?;
        if asset.size > 0 && asset.size != part.size {
            return Err(Error::msg(format!(
                "part {} is {} bytes on GitHub but {} in the manifest",
                part.name, asset.size, part.size
            )));
        }
    }
    Ok(())
}

pub struct SplitDownload<'a> {
    pub client: &'a reqwest::Client,
    pub asset: &'a ReleaseAsset,
    /// Where the joined zip ends up.
    pub dest: &'a Path,
    /// Folder for the parts; they are kept between runs so a retry resumes.
    pub parts_dir: &'a Path,
    pub token: Option<&'a str>,
}

/// Download every part (resuming), verify each, join, verify the whole file,
/// and check that it is a readable zip. The parts are removed after success.
pub async fn download_split<F>(request: SplitDownload<'_>, mut progress: F) -> Result<PartsManifest>
where
    F: FnMut(PatchProgress),
{
    let split = request
        .asset
        .split
        .as_deref()
        .ok_or_else(|| Error::msg("asset is not split"))?;
    fs::create_dir_all(request.parts_dir)?;
    let manifest_path = request
        .parts_dir
        .join(format!("{}{PARTS_MANIFEST_SUFFIX}", file_only(&request.asset.name)));
    let _ = fs::remove_file(&manifest_path);
    let url = download_url(&split.manifest)?.to_string();
    download_with_resume(
        DownloadRequest {
            client: request.client,
            url: &url,
            dest: &manifest_path,
            expected_size: split.manifest.size,
            token: request.token,
        },
        |_| {},
    )
    .await?;
    let manifest: PartsManifest = serde_json::from_str(&fs::read_to_string(&manifest_path)?)
        .map_err(|err| Error::msg(format!("parts manifest is not valid JSON: {err}")))?;
    validate_manifest(&manifest, split)?;

    let total = manifest.size;
    let mut done: u64 = 0;
    let mut part_paths: Vec<PathBuf> = Vec::new();
    for (index, part) in manifest.parts.iter().enumerate() {
        let asset = split
            .parts
            .iter()
            .find(|a| a.name == part.name)
            .expect("validated");
        let path = request.parts_dir.join(file_only(&part.name));
        let want = clean_hex(&part.sha256);
        let already = fs::metadata(&path).map(|m| m.len() == part.size).unwrap_or(false)
            && sha256_file(&path).map(|h| h == want).unwrap_or(false);
        if !already {
            let part_url = download_url(asset)?.to_string();
            let mut last_err = String::new();
            let mut ok = false;
            for attempt in 1..=PART_ATTEMPTS {
                let offset = done;
                let label = format!(
                    "Downloading part {} of {}",
                    index + 1,
                    manifest.parts.len()
                );
                let result = download_with_resume(
                    DownloadRequest {
                        client: request.client,
                        url: &part_url,
                        dest: &path,
                        expected_size: part.size,
                        token: request.token,
                    },
                    |p| {
                        let received = offset + p.received.min(part.size);
                        progress(PatchProgress {
                            phase: "download".into(),
                            received,
                            total,
                            percent: if total == 0 { 0 } else { ((received * 100) / total).min(100) as u8 },
                            message: label.clone(),
                        })
                    },
                )
                .await;
                match result {
                    Ok(_) => {
                        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        if len != part.size {
                            last_err = format!("part {} is {len} bytes, expected {}", part.name, part.size);
                            if len > part.size {
                                let _ = fs::remove_file(&path);
                            }
                            continue;
                        }
                        if sha256_file(&path)? == want {
                            ok = true;
                            break;
                        }
                        last_err = format!("part {} failed its checksum", part.name);
                        let _ = fs::remove_file(&path);
                    }
                    // Keep the partial file so the next attempt resumes.
                    Err(err) => last_err = err.to_string(),
                }
                tracing::warn!("part {} attempt {attempt} failed: {last_err}", part.name);
            }
            if !ok {
                return Err(Error::msg(format!(
                    "{last_err} (after {PART_ATTEMPTS} attempts). Downloaded parts are kept; try again to resume."
                )));
            }
        }
        done += part.size;
        part_paths.push(path);
    }

    progress(PatchProgress {
        phase: "verify".into(),
        received: total,
        total,
        percent: 100,
        message: "Joining parts".into(),
    });
    let joined = join_parts(&part_paths, request.dest)?;
    if joined.0 != manifest.size || joined.1 != clean_hex(&manifest.sha256) {
        let _ = fs::remove_file(request.dest);
        return Err(Error::msg(
            "joined file does not match the parts manifest (size or sha256)",
        ));
    }
    // Size and hash are already checked; this only proves the zip is readable.
    verify_download(request.dest, manifest.size, None)?;
    for path in &part_paths {
        let _ = fs::remove_file(path);
    }
    let _ = fs::remove_file(&manifest_path);
    Ok(manifest)
}

/// Concatenate parts into `dest` (through a temp file) and return (size, sha256).
pub fn join_parts(parts: &[PathBuf], dest: &Path) -> Result<(u64, String)> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_file_name(format!("{}.joining", file_only(&dest.display().to_string())));
    let mut out = File::create(&tmp)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buf = vec![0u8; 1024 * 1024];
    for part in parts {
        let mut input = File::open(part)?;
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            size += n as u64;
        }
    }
    out.sync_all()?;
    drop(out);
    if dest.exists() {
        fs::remove_file(dest)?;
    }
    fs::rename(&tmp, dest)?;
    Ok((size, hex::encode(hasher.finalize())))
}

/// Split `file` into `<name>.001`, `<name>.002`, ... in `out_dir` and write
/// `<name>.parts.json`. Used by tests and by publish tooling.
pub fn split_file(file: &Path, out_dir: &Path, part_size: u64) -> Result<PartsManifest> {
    if part_size == 0 {
        return Err(Error::msg("part size must be above zero"));
    }
    fs::create_dir_all(out_dir)?;
    let name = file_only(&file.display().to_string());
    let mut input = File::open(file)?;
    let mut whole = Sha256::new();
    let mut parts = Vec::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut size = 0u64;
    let mut index = 1u32;
    loop {
        let part_name = format!("{name}.{index:03}");
        let part_path = out_dir.join(&part_name);
        let mut out = File::create(&part_path)?;
        let mut hasher = Sha256::new();
        let mut written = 0u64;
        while written < part_size {
            let want = (part_size - written).min(buf.len() as u64) as usize;
            let n = input.read(&mut buf[..want])?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            whole.update(&buf[..n]);
            written += n as u64;
        }
        drop(out);
        if written == 0 {
            let _ = fs::remove_file(&part_path);
            break;
        }
        size += written;
        parts.push(PartEntry {
            name: part_name,
            size: written,
            sha256: hex::encode(hasher.finalize()),
        });
        index += 1;
        if written < part_size {
            break;
        }
    }
    let manifest = PartsManifest {
        version: 1,
        file: name.clone(),
        size,
        sha256: hex::encode(whole.finalize()),
        parts,
    };
    fs::write(
        out_dir.join(format!("{name}{PARTS_MANIFEST_SUFFIX}")),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    Ok(manifest)
}

fn file_only(name: &str) -> String {
    Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download.zip".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str, size: u64) -> ReleaseAsset {
        ReleaseAsset {
            name: name.into(),
            api_url: String::new(),
            browser_download_url: format!("http://x/{name}"),
            size,
            digest: None,
            split: None,
        }
    }

    #[test]
    fn part_numbers() {
        assert_eq!(part_number("A.zip.001"), Some(("A.zip", 1)));
        assert_eq!(part_number("A.zip.012"), Some(("A.zip", 12)));
        assert_eq!(part_number("A.zip"), None);
        assert_eq!(part_number("A.zip.1"), None);
    }

    #[test]
    fn finds_split_asset_in_part_order() {
        let assets = vec![
            asset("FNF_Client_Win64.zip.002", 5),
            asset("FNF_Client_Win64.zip.parts.json", 300),
            asset("FNF_Client_Win64.zip.001", 10),
            asset("FNF_Server_Win64.zip", 7),
        ];
        let found = find_split_asset(&assets, |n| n.contains("Client")).unwrap();
        assert_eq!(found.name, "FNF_Client_Win64.zip");
        assert_eq!(found.size, 15);
        let split = found.split.unwrap();
        assert_eq!(split.parts[0].name, "FNF_Client_Win64.zip.001");
        assert_eq!(split.parts[1].name, "FNF_Client_Win64.zip.002");
        assert!(find_split_asset(&assets, |n| n.contains("Server")).is_none());
    }

    #[test]
    fn split_then_join_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("big.zip");
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &data).unwrap();
        let out = dir.path().join("out");
        let manifest = split_file(&src, &out, 4096).unwrap();
        assert_eq!(manifest.parts.len(), 3);
        assert_eq!(manifest.size, 10_000);
        assert_eq!(manifest.parts[2].size, 10_000 - 8192);
        let paths: Vec<PathBuf> = manifest.parts.iter().map(|p| out.join(&p.name)).collect();
        let joined = dir.path().join("joined.zip");
        let (size, sha) = join_parts(&paths, &joined).unwrap();
        assert_eq!(size, 10_000);
        assert_eq!(sha, manifest.sha256);
        assert_eq!(fs::read(&joined).unwrap(), data);
    }

    #[test]
    fn exact_multiple_has_no_empty_part() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("e.zip");
        fs::write(&src, vec![7u8; 8192]).unwrap();
        let manifest = split_file(&src, &dir.path().join("o"), 4096).unwrap();
        assert_eq!(manifest.parts.len(), 2);
        assert!(!dir.path().join("o/e.zip.003").exists());
    }
}
