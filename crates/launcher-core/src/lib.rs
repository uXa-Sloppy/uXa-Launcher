//! Release lookup, safe patching, and launch planning for the uXa launcher.
//! The desktop shell lives in the Tauri crate; this library stays UI-free so
//! the install path can be tested without a window.

mod error;
mod github;
mod inspect;
mod launch;
mod model;
mod patch;
mod version;
mod web;

pub use error::{Error, Result};
pub use github::{
    check_self_update, download_url, http_client, CheckUpdate, ReleaseAsset, ReleaseInfo,
    ReleaseSource,
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
    asset_matches, atomic_swap, check_zip_magic, copy_preserved, download_with_resume,
    extract_zip, patch_game, verify_download, DownloadRequest, PatchJob, PatchProgress,
};
pub use version::{is_newer, parse_version};
pub use web::{fetch_web_build, parse_web_build, WebBuild};

pub const USER_AGENT: &str = "uXa-Launcher/0.1";
pub const DEFAULT_WEB_VERSION_PATTERN: &str =
    r"Build\s+(\d+)\s*,\s*last updated\s+([^<\r\n]+)";
