use crate::github::{find_latest_release, ReleaseSource};
use crate::model::{GameDef, GameSource, GameStatus, InstalledManifest};
use crate::version::is_newer;
use crate::web::fetch_web_build;
use crate::Result;

pub struct InspectRequest<'a> {
    pub client: &'a reqwest::Client,
    pub game: &'a GameDef,
    pub manifest: Option<&'a InstalledManifest>,
    pub api_base: &'a str,
    pub token: Option<&'a str>,
    pub remote: bool,
}

pub async fn inspect_game(request: InspectRequest<'_>) -> Result<GameStatus> {
    let installed = request
        .manifest
        .map(|m| m.version.clone())
        .filter(|v| !v.is_empty());
    match &request.game.source {
        GameSource::Web { url, version_pattern } => {
            if !request.remote {
                return Ok(status(
                    request.game,
                    "ready",
                    installed,
                    None,
                    None,
                    String::new(),
                    String::new(),
                    None,
                    None,
                    Some(url.clone()),
                ));
            }
            match fetch_web_build(request.client, url, version_pattern).await {
                Ok(build) => Ok(status(
                    request.game,
                    "ready",
                    Some(format!("Build {}", build.build)),
                    Some(format!("Build {}", build.build)),
                    Some(build.line.clone()),
                    build.line,
                    String::new(),
                    Some(build.build),
                    Some(build.updated),
                    Some(url.clone()),
                )),
                Err(err) => Ok(status(
                    request.game,
                    "ready",
                    installed,
                    None,
                    None,
                    String::new(),
                    format!("Could not read the build number ({err}). You can still play."),
                    None,
                    None,
                    Some(url.clone()),
                )),
            }
        }
        GameSource::GithubRelease {
            owner,
            repo,
            asset_contains,
            asset_pattern,
            include_prereleases,
        } => {
            if !request.remote {
                let state = if installed.is_some() {
                    "ready"
                } else {
                    "not-installed"
                };
                return Ok(status(
                    request.game,
                    state,
                    installed,
                    None,
                    None,
                    String::new(),
                    String::new(),
                    None,
                    None,
                    None,
                ));
            }
            let source = ReleaseSource {
                api_base: request.api_base.to_string(),
                owner: owner.clone(),
                repo: repo.clone(),
                asset_contains: asset_contains.clone(),
                asset_pattern: asset_pattern.clone(),
                include_prereleases: *include_prereleases,
                token: request.token.map(str::to_string),
            };
            match find_latest_release(request.client, &source).await {
                Ok(release) => {
                    let newer = is_newer(&release.tag, installed.as_deref()).unwrap_or(true);
                    let state = if installed.is_none() {
                        "not-installed"
                    } else if newer {
                        "update-available"
                    } else {
                        "ready"
                    };
                    let name = if release.name.trim().is_empty() {
                        release.tag.clone()
                    } else {
                        release.name
                    };
                    Ok(status(
                        request.game,
                        state,
                        installed,
                        Some(release.tag),
                        Some(name),
                        release.body,
                        String::new(),
                        None,
                        None,
                        None,
                    ))
                }
                Err(err) => {
                    let state = if installed.is_some() {
                        "ready"
                    } else {
                        "check-failed"
                    };
                    Ok(status(
                        request.game,
                        state,
                        installed,
                        None,
                        None,
                        String::new(),
                        err.to_string(),
                        None,
                        None,
                        None,
                    ))
                }
            }
        }
    }
}

fn status(
    game: &GameDef,
    state: &str,
    installed_version: Option<String>,
    remote_version: Option<String>,
    release_name: Option<String>,
    patch_notes: String,
    message: String,
    web_build: Option<String>,
    web_updated: Option<String>,
    web_url: Option<String>,
) -> GameStatus {
    GameStatus {
        game_id: game.id.clone(),
        state: state.to_string(),
        installed_version,
        remote_version,
        release_name,
        patch_notes,
        message,
        web_build,
        web_updated,
        web_url,
    }
}
