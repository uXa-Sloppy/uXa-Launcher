use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::version::is_newer;
use crate::{Error, Result, USER_AGENT as UA};

#[derive(Debug, Clone)]
pub struct ReleaseSource {
    pub api_base: String,
    pub owner: String,
    pub repo: String,
    pub asset_contains: Vec<String>,
    pub asset_pattern: String,
    pub include_prereleases: bool,
    pub token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReleaseAsset {
    pub name: String,
    pub api_url: String,
    pub browser_download_url: String,
    pub size: u64,
    pub digest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    pub tag: String,
    pub name: String,
    pub body: String,
    pub prerelease: bool,
    pub html_url: String,
    pub asset: ReleaseAsset,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckUpdate {
    pub latest: String,
    pub url: String,
    pub notes: String,
    /// The launcher .exe attached to the release, when there is one (enables in-place self-update).
    pub asset: Option<LauncherAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LauncherAsset {
    pub name: String,
    pub api_url: String,
    pub size: u64,
    pub digest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(UA)
        .redirect(reqwest::redirect::Policy::limited(10))
        .connect_timeout(Duration::from_secs(20))
        .build()?)
}

fn headers(token: Option<&str>, accept: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(UA));
    headers.insert(ACCEPT, HeaderValue::from_str(accept).map_err(|e| Error::msg(e.to_string()))?);
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static("2022-11-28"),
    );
    if let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Error::msg("GitHub token contains characters that are not allowed in a header"))?;
        headers.insert(AUTHORIZATION, value);
    }
    Ok(headers)
}

pub(crate) fn auth_headers(token: Option<&str>, accept: &str) -> Result<HeaderMap> {
    headers(token, accept)
}

fn friendly_status(status: StatusCode, body: &str) -> String {
    let detail = body.chars().take(180).collect::<String>().replace('\n', " ");
    match status.as_u16() {
        401 | 403 => {
            "GitHub refused the request. If the repository is private, add a personal access token with contents access in Settings.".into()
        }
        404 => {
            "No visible GitHub releases. If the repository is private, add a token in Settings.".into()
        }
        _ if detail.is_empty() => format!("GitHub returned {status}"),
        _ => format!("GitHub returned {status}: {detail}"),
    }
}

fn next_page(link: &str) -> Option<String> {
    for part in link.split(',') {
        if part.contains("rel=\"next\"") {
            let start = part.find('<')? + 1;
            let end = part.find('>')?;
            return Some(part[start..end].to_string());
        }
    }
    None
}

fn into_release(release: GhRelease, asset: GhAsset) -> ReleaseInfo {
    tracing::info!("selected release {} asset {}", release.tag_name, asset.name);
    ReleaseInfo {
        tag: release.tag_name,
        name: release.name.unwrap_or_default(),
        body: release.body.unwrap_or_default(),
        prerelease: release.prerelease,
        html_url: release.html_url.unwrap_or_default(),
        asset: ReleaseAsset {
            name: asset.name,
            api_url: asset.url,
            browser_download_url: asset.browser_download_url,
            size: asset.size,
            digest: asset.digest.filter(|d| !d.is_empty()),
        },
    }
}

/// Every non-draft release (newest first, up to five pages) that has a matching zip.
pub async fn list_matching_releases(
    client: &reqwest::Client,
    source: &ReleaseSource,
) -> Result<Vec<ReleaseInfo>> {
    let pattern = if source.asset_pattern.trim().is_empty() {
        None
    } else {
        Some(regex::Regex::new(source.asset_pattern.trim())?)
    };
    let mut url = format!(
        "{}/repos/{}/{}/releases?per_page=30",
        source.api_base.trim_end_matches('/'),
        source.owner,
        source.repo
    );
    let headers = headers(source.token.as_deref(), "application/vnd.github+json")?;
    let mut found = Vec::new();

    for _ in 0..5 {
        let response = client.get(&url).headers(headers.clone()).send().await?;
        let status = response.status();
        let link = response
            .headers()
            .get("link")
            .and_then(|v| v.to_str().ok())
            .and_then(next_page);
        let text = response.text().await?;
        if !status.is_success() {
            return Err(Error::msg(friendly_status(status, &text)));
        }
        let releases: Vec<GhRelease> = serde_json::from_str(&text)?;
        for mut release in releases {
            if release.draft {
                continue;
            }
            if release.prerelease && !source.include_prereleases {
                continue;
            }
            let assets = std::mem::take(&mut release.assets);
            if let Some(asset) = assets.into_iter().find(|asset| {
                crate::patch::asset_matches(&asset.name, &source.asset_contains, pattern.as_ref())
            }) {
                found.push(into_release(release, asset));
                if found.len() >= 40 {
                    return Ok(found);
                }
            }
        }
        match link {
            Some(next) => url = next,
            None => break,
        }
    }
    Ok(found)
}

pub async fn find_latest_release(
    client: &reqwest::Client,
    source: &ReleaseSource,
) -> Result<ReleaseInfo> {
    list_matching_releases(client, source)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
            Error::msg(
                "No release asset matched the configured name filter (expected a zip whose name contains the configured tokens, default Client and Win64).",
            )
        })
}

pub async fn find_release_by_tag(
    client: &reqwest::Client,
    source: &ReleaseSource,
    tag: &str,
) -> Result<ReleaseInfo> {
    let tag = tag.trim();
    if tag.is_empty() {
        return find_latest_release(client, source).await;
    }
    list_matching_releases(client, source)
        .await?
        .into_iter()
        .find(|release| release.tag == tag)
        .ok_or_else(|| {
            Error::msg(format!(
                "No release {tag} has a zip matching the configured name filter."
            ))
        })
}

/// Launcher self-update check. Empty owner/repo yields `Ok(None)` so v0.1 can ship before its own feed exists.
pub async fn check_self_update(
    client: &reqwest::Client,
    api_base: &str,
    owner: &str,
    repo: &str,
    current: &str,
    token: Option<&str>,
) -> Result<Option<CheckUpdate>> {
    if owner.trim().is_empty() || repo.trim().is_empty() {
        return Ok(None);
    }
    let url = format!(
        "{}/repos/{}/{}/releases/latest",
        api_base.trim_end_matches('/'),
        owner.trim(),
        repo.trim()
    );
    let response = client
        .get(&url)
        .headers(headers(token, "application/vnd.github+json")?)
        .send()
        .await?;
    let status = response.status();
    if status.as_u16() == 404 {
        return Ok(None);
    }
    let text = response.text().await?;
    if !status.is_success() {
        return Err(Error::msg(friendly_status(status, &text)));
    }
    let release: GhRelease = serde_json::from_str(&text)?;
    if !is_newer(&release.tag_name, Some(current))? {
        return Ok(None);
    }
    Ok(Some(CheckUpdate {
        latest: release.tag_name,
        url: release.html_url.unwrap_or_default(),
        notes: release.body.unwrap_or_default(),
        asset: pick_launcher_asset(&release.assets),
    }))
}

pub fn download_url(asset: &ReleaseAsset) -> Result<&str> {
    if !asset.api_url.trim().is_empty() {
        return Ok(asset.api_url.trim());
    }
    if !asset.browser_download_url.trim().is_empty() {
        return Ok(asset.browser_download_url.trim());
    }
    Err(Error::msg(format!("asset {} has no download URL", asset.name)))
}

/// The launcher executable on a release: a `.exe` whose name contains "launcher" (not an installer or the agent).
fn pick_launcher_asset(assets: &[GhAsset]) -> Option<LauncherAsset> {
    assets
        .iter()
        .find(|a| {
            let n = a.name.to_ascii_lowercase();
            n.ends_with(".exe") && n.contains("launcher") && !n.contains("setup") && !n.contains("agent")
        })
        .map(|a| LauncherAsset {
            name: a.name.clone(),
            api_url: a.url.clone(),
            size: a.size,
            digest: a.digest.clone().filter(|d| !d.is_empty()),
        })
}

/// Download a launcher asset with the existing read-only token, then verify size and sha256 digest.
pub async fn download_launcher_asset(
    client: &reqwest::Client,
    asset: &LauncherAsset,
    token: Option<&str>,
    dest: &std::path::Path,
) -> Result<()> {
    let response = client
        .get(&asset.api_url)
        .headers(headers(token, "application/octet-stream")?)
        .timeout(Duration::from_secs(600))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(Error::msg(friendly_status(status, &text)));
    }
    let bytes = response.bytes().await?;
    if asset.size > 0 && bytes.len() as u64 != asset.size {
        return Err(Error::msg(format!(
            "launcher download is {} bytes, release says {}",
            bytes.len(),
            asset.size
        )));
    }
    std::fs::write(dest, &bytes)?;
    match &asset.digest {
        Some(d) => {
            if let Err(e) = crate::patch::check_digest(dest, d) {
                let _ = std::fs::remove_file(dest);
                return Err(e);
            }
        }
        None => {
            let _ = std::fs::remove_file(dest);
            return Err(Error::msg("release asset has no sha256 digest; refusing to self-update"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod launcher_asset_tests {
    use super::*;
    fn a(name: &str) -> GhAsset {
        GhAsset { name: name.into(), url: "u".into(), browser_download_url: String::new(), size: 1, digest: None }
    }
    #[test]
    fn picks_launcher_exe_only() {
        let list = vec![a("uXa-Server-Agent.exe"), a("uXa-Launcher_setup.exe"), a("uXa-Launcher.exe"), a("game.zip")];
        assert_eq!(pick_launcher_asset(&list).unwrap().name, "uXa-Launcher.exe");
        assert!(pick_launcher_asset(&[a("game.zip")]).is_none());
    }
}
