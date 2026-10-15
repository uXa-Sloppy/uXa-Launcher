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
mod sign;
mod version;
mod web;

pub use agent::{
    agent_exchange, AgentAction, AgentLogLine, AgentReleaseSummary, AgentStatus,
};
pub use error::{Error, Result};
pub use github::{
    check_self_update, download_url, find_release_by_tag, http_client, list_matching_releases,
    download_launcher_asset, CheckUpdate, LauncherAsset, ReleaseAsset, ReleaseInfo, ReleaseSource,
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
pub use sign::{sign_request, verify_request, MAX_SKEW_SECS};
pub use version::{is_newer, parse_version};
pub use web::{fetch_web_build, parse_web_build, WebBuild};

pub const USER_AGENT: &str = "uXa-Launcher/0.2";
pub const DEFAULT_WEB_VERSION_PATTERN: &str =
    r"Build\s+(\d+)\s*,\s*last updated\s+([^<\r\n]+)";
