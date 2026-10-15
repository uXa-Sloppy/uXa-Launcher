//! Release lookup, safe patching, and launch planning for the uXa launcher.
//! The desktop shell lives in the Tauri crate; this library stays UI-free so
//! the install path can be tested without a window.

mod agent;
mod error;
mod github;
mod inspect;
mod launch;
mod model;
mod patch;
mod self_update;
mod sign;
mod version;
mod web;

pub use agent::{agent_exchange, AgentAction, AgentLogLine, AgentReleaseSummary, AgentStatus};
pub use error::{Error, Result};
pub use github::{
    check_self_update, download_url, fetch_latest_release, find_release_by_tag, http_client,
    list_matching_releases, repo_visibility, CheckUpdate, FullRelease, ReleaseAsset, ReleaseInfo,
    ReleaseSource, RepoVisibility,
};
pub use inspect::{inspect_game, InspectRequest};
pub use launch::{
    find_executable, format_address, format_servers_ini, prepare_client_launch, probe_tcp,
    spawn_process, validate_server, write_servers_ini, ClientLaunch, PlayPlan,
};
pub use model::{
    load_catalog, load_news, load_servers, load_settings, read_manifest, resolve_install_dir,
    save_settings, write_manifest, Catalog, GameDef, GameSource, GameStatus, InstalledManifest,
    LauncherInfo, NewsItem, ServerEntry, ServerMap, Settings,
};
pub use patch::{
    asset_matches, atomic_swap, check_zip_magic, copy_preserved, discard_previous,
    download_with_resume, extract_zip, patch_game, previous_install_dir, restore_previous,
    swap_keeping_previous, verify_download, DownloadRequest, PatchJob, PatchProgress,
};
pub use self_update::{
    backup_path, cleanup_previous_launcher, download_launcher_update, file_sha256,
    installed_launcher_path, parse_sha256sums, replace_executable, resolve_update_repo,
    restore_previous_launcher, running_from_cargo_target, select_launcher_exe,
    token_for_visibility, verify_exe_file, LauncherUpdateFetch, PreparedLauncherUpdate,
    CHECKSUM_ASSET, DEFAULT_UPDATE_OWNER, DEFAULT_UPDATE_REPO, INSTALL_DIR_NAME,
    LAUNCHER_EXE_ASSET,
};
pub use sign::{sign_request, verify_request, MAX_SKEW_SECS};
pub use version::{is_newer, parse_version};
pub use web::{fetch_web_build, parse_web_build, WebBuild};

pub const USER_AGENT: &str = "uXa-Launcher/0.3";
pub const DEFAULT_WEB_VERSION_PATTERN: &str = r"Build\s+(\d+)\s*,\s*last updated\s+([^<\r\n]+)";
