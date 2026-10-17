//! Headless commands for deploy, client patch, and status.
//! This module only reads the admin key and the GitHub token. It never writes
//! settings, `servers.json`, the admin key, or the token.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::agent::agent_exchange;
use crate::model::{
    load_catalog, load_servers, load_settings, read_manifest, resolve_install_dir, write_manifest,
    GameDef, GameSource, ServerEntry, Settings,
};
use crate::patch::{patch_game, PatchJob};
use crate::self_update::redact_secrets;
use crate::ReleaseSource;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

pub const FNF_GAME_ID: &str = "friday-night-fights";

pub const MISSING_ADMIN_KEY: &str = "No admin key is saved. Unlock admin once in the launcher window so it is remembered on this PC.";

const PROTECTED_FILES: &[&str] = &[
    "settings.json",
    "servers.json",
    "agent-secret",
    "github-token",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliCommand {
    Help,
    Version,
    Status,
    UpdateClient,
    DeployServer { version: String },
}

pub struct CliInput<'a> {
    pub launcher_version: &'a str,
    pub data_dir: &'a Path,
    pub install_root: &'a Path,
    pub games_json: &'a str,
    pub servers_json: &'a str,
    pub api_base: &'a str,
    pub agent_url_override: Option<&'a str>,
    pub admin_key: Option<&'a str>,
    pub github_token: Option<&'a str>,
}

pub struct CliReport {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub versions: CliVersions,
}

/// Versions an automation script can read without parsing the console text.
/// `client` and `server` are null when that command did not learn them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliVersions {
    pub launcher: String,
    pub client: Option<String>,
    pub server: Option<String>,
}

impl CliVersions {
    fn empty() -> Self {
        Self {
            launcher: String::new(),
            client: None,
            server: None,
        }
    }
}

/// One CLI run, written to `cli-last.json` for scripts that cannot see a console.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliResultFile {
    pub command: String,
    pub ok: bool,
    pub message: String,
    pub versions: CliVersions,
    pub timestamp: String,
    pub exit_code: i32,
}

/// What a Windows GUI-subsystem process inherited for one standard handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritedHandle {
    /// No handle. A release build launched from a console looks like this
    /// unless the shell redirected that stream.
    Missing,
    /// Already a console. Debug builds, and `AllocConsole`, land here.
    Console,
    /// A pipe or file. `> out.txt` and `|` must keep this handle.
    Redirected,
}

/// Whether to attach a console and which standard handles may be replaced.
/// Replacing a redirected handle is what made `--version > file` empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdioPlan {
    pub attach_console: bool,
    pub replace_stdout: bool,
    pub replace_stderr: bool,
}

pub fn stdio_plan(stdout: InheritedHandle, stderr: InheritedHandle) -> StdioPlan {
    let replace_stdout = matches!(stdout, InheritedHandle::Missing);
    let replace_stderr = matches!(stderr, InheritedHandle::Missing);
    StdioPlan {
        attach_console: replace_stdout || replace_stderr,
        replace_stdout,
        replace_stderr,
    }
}

pub fn command_label(command: &CliCommand) -> String {
    match command {
        CliCommand::Help => "--help".into(),
        CliCommand::Version => "--version".into(),
        CliCommand::Status => "--status".into(),
        CliCommand::UpdateClient => "--update-client".into(),
        CliCommand::DeployServer { version } => format!("--deploy-server {version}"),
    }
}

pub fn cli_result_record(command: &str, report: &CliReport) -> CliResultFile {
    CliResultFile {
        command: command.to_string(),
        ok: report.exit_code == 0,
        message: redact_secrets(&result_message(report)),
        versions: report.versions.clone(),
        timestamp: utc_timestamp(),
        exit_code: report.exit_code,
    }
}

pub fn usage_cli_result(launcher_version: &str, command: &str, message: &str) -> CliResultFile {
    CliResultFile {
        command: command.to_string(),
        ok: false,
        message: redact_secrets(message.trim()),
        versions: CliVersions {
            launcher: launcher_version.to_string(),
            client: None,
            server: None,
        },
        timestamp: utc_timestamp(),
        exit_code: EXIT_USAGE,
    }
}

pub fn failure_cli_result(
    command: &str,
    launcher_version: &str,
    message: &str,
    exit_code: i32,
) -> CliResultFile {
    CliResultFile {
        command: command.to_string(),
        ok: false,
        message: redact_secrets(message.trim()),
        versions: CliVersions {
            launcher: launcher_version.to_string(),
            client: None,
            server: None,
        },
        timestamp: utc_timestamp(),
        exit_code,
    }
}

/// Replace `path` with the result. A reader never sees a half-written file.
pub fn write_cli_result(path: &Path, result: &CliResultFile) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    let mut result = result.clone();
    result.message = redact_secrets(&result.message);
    let mut body = serde_json::to_vec_pretty(&result).map_err(|err| err.to_string())?;
    if !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &body).map_err(|err| format!("Could not write {}: {err}", tmp.display()))?;
    if path.exists() {
        fs::remove_file(path)
            .map_err(|err| format!("Could not replace {}: {err}", path.display()))?;
    }
    fs::rename(&tmp, path).map_err(|err| {
        let _ = fs::remove_file(&tmp);
        format!("Could not write {}: {err}", path.display())
    })?;
    Ok(())
}

fn result_message(report: &CliReport) -> String {
    let stdout = report.stdout.trim();
    let stderr = report.stderr.trim();
    match (report.exit_code == 0, stdout.is_empty(), stderr.is_empty()) {
        (true, _, _) => stdout.to_string(),
        (false, true, _) => stderr.to_string(),
        (false, false, true) => stdout.to_string(),
        (false, false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn utc_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

pub fn parse_cli(args: &[String]) -> Result<Option<CliCommand>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(Some(CliCommand::Help));
    }
    let mut command = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let parsed = match arg {
            "--version" | "-V" => CliCommand::Version,
            "--status" => CliCommand::Status,
            "--update-client" => CliCommand::UpdateClient,
            "--deploy-server" => {
                let version = args.get(index + 1).map(String::as_str).unwrap_or("");
                if version.is_empty() || version.starts_with('-') {
                    return Err("--deploy-server needs a version, for example v1.2.0.".into());
                }
                index += 1;
                CliCommand::DeployServer {
                    version: version.to_string(),
                }
            }
            other => {
                return Err(format!(
                    "Unknown argument {other}. Run with --help for usage."
                ));
            }
        };
        if command.is_some() {
            return Err("Pass one command at a time. Run with --help for usage.".into());
        }
        command = Some(parsed);
        index += 1;
    }
    Ok(command)
}

pub fn help_text(version: &str) -> String {
    format!(
        "\
uXa Launcher {version}

Usage:
  uxa-launcher
  uxa-launcher --status
  uxa-launcher --update-client
  uxa-launcher --deploy-server <version>
  uxa-launcher --version
  uxa-launcher --help

With no flags, the launcher window opens. These commands print progress and
do not open a window. They still run when the window is already open.

--status
    Print the launcher version, the installed Friday Night Fights client
    version, and the selected server's version and online state.

--update-client
    Download and install the latest Friday Night Fights client patch.
    The game is not started.

--deploy-server <version>
    Deploy that Friday Night Fights server version (a release tag such as
    v1.2.0) to the selected server. Dolmar's Server is the default. This uses
    the admin key already saved by the launcher window.

The server list is the bundled config, or %LOCALAPPDATA%\\uXa\\Launcher\\servers.json
when that file is present. Deploy and status use the saved admin key. Unlock
admin once in the launcher window if the key is not saved yet.

Every command also writes %LOCALAPPDATA%\\uXa Launcher\\cli-last.json with
command, ok, message, versions, timestamp, and exitCode. Success exits 0.
A failed command exits 1. A bad argument exits 2.
"
    )
}

pub fn redact_cli(text: &str, secrets: &[&str]) -> String {
    let mut out = redact_secrets(text);
    for secret in secrets {
        let secret = secret.trim();
        if secret.len() >= 4 {
            out = out.replace(secret, "[redacted]");
        }
    }
    out
}

pub async fn execute_cli(
    client: &reqwest::Client,
    input: &CliInput<'_>,
    command: CliCommand,
) -> CliReport {
    if let Some(report) = try_local_cli(input, &command) {
        return report;
    }
    let guard = ProtectedSnapshot::capture(input.data_dir);
    let report = match command {
        CliCommand::Help | CliCommand::Version => {
            fail_report(String::new(), "This command could not be run.")
        }
        CliCommand::Status => status(client, input).await,
        CliCommand::UpdateClient => update_client(client, input).await,
        CliCommand::DeployServer { version } => deploy(client, input, &version).await,
    };
    finish(input, guard, report)
}

fn finish(input: &CliInput<'_>, guard: ProtectedSnapshot, mut report: CliReport) -> CliReport {
    let secrets = secret_list(input);
    report.stdout = redact_cli(&report.stdout, &secrets);
    report.stderr = redact_cli(&report.stderr, &secrets);
    if guard.changed() {
        report.exit_code = EXIT_FAILURE;
        report.stderr.push_str(
            "The command would have changed saved launcher settings. Those files were restored.\n",
        );
    }
    if report.versions.launcher.is_empty() {
        report.versions.launcher = input.launcher_version.to_string();
    }
    report
}

/// Help and version do not touch the network, the admin key, or settings.
pub fn try_local_cli(input: &CliInput<'_>, command: &CliCommand) -> Option<CliReport> {
    let stdout = match command {
        CliCommand::Help => help_text(input.launcher_version),
        CliCommand::Version => format!("uXa Launcher {}\n", input.launcher_version),
        _ => return None,
    };
    let mut report = ok_report(stdout);
    report.versions.launcher = input.launcher_version.to_string();
    Some(report)
}

fn secret_list<'a>(input: &'a CliInput<'a>) -> Vec<&'a str> {
    [input.admin_key, input.github_token]
        .into_iter()
        .flatten()
        .collect()
}

fn ok_report(stdout: String) -> CliReport {
    CliReport {
        exit_code: EXIT_OK,
        stdout,
        stderr: String::new(),
        versions: CliVersions::empty(),
    }
}

fn fail_report(stdout: String, stderr: impl Into<String>) -> CliReport {
    let mut stderr = stderr.into();
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    CliReport {
        exit_code: EXIT_FAILURE,
        stdout,
        stderr,
        versions: CliVersions::empty(),
    }
}

async fn status(client: &reqwest::Client, input: &CliInput<'_>) -> CliReport {
    let mut stdout = format!("Launcher: {}\n", input.launcher_version);
    let mut client_version = None;
    let loaded = match load_context(input) {
        Ok(loaded) => loaded,
        Err(err) => return known(input, fail_report(stdout, err), None, None),
    };
    match read_manifest(&manifest_path(input.data_dir)) {
        Ok(Some(manifest)) => {
            client_version = Some(manifest.version.clone());
            stdout.push_str(&format!("Client: {}\n", manifest.version));
        }
        Ok(None) => stdout.push_str("Client: not installed\n"),
        Err(err) => {
            return known(
                input,
                fail_report(
                    stdout,
                    format!("Could not read the installed client: {err}"),
                ),
                None,
                None,
            )
        }
    }
    let server = match pick_server(&loaded.settings, &loaded.servers) {
        Ok(server) => server,
        Err(err) => return known(input, fail_report(stdout, err), client_version, None),
    };
    stdout.push_str(&format!("Server: {}\n", server.name));
    let secret = match saved_secret(input.admin_key) {
        Some(secret) => secret,
        None => {
            return known(
                input,
                fail_report(stdout, MISSING_ADMIN_KEY),
                client_version,
                None,
            )
        }
    };
    let base = match agent_base(server, input.agent_url_override) {
        Ok(base) => base,
        Err(err) => return known(input, fail_report(stdout, err), client_version, None),
    };
    match agent_status(client, &base, secret).await {
        Ok(agent) => {
            stdout.push_str(&format!("Server version: {}\n", agent.version));
            stdout.push_str(&format!("Server online: {}\n", yes_no(agent.online)));
            known(
                input,
                ok_report(stdout),
                client_version,
                Some(agent.version),
            )
        }
        Err(err) => known(input, fail_report(stdout, err), client_version, None),
    }
}

fn known(
    input: &CliInput<'_>,
    mut report: CliReport,
    client_version: Option<String>,
    server_version: Option<String>,
) -> CliReport {
    report.versions.launcher = input.launcher_version.to_string();
    report.versions.client = client_version;
    report.versions.server = server_version;
    report
}

async fn update_client(client: &reqwest::Client, input: &CliInput<'_>) -> CliReport {
    let loaded = match load_context(input) {
        Ok(loaded) => loaded,
        Err(err) => return fail_report(String::new(), err),
    };
    let game = match fnf_game(&loaded.catalog) {
        Ok(game) => game,
        Err(err) => return fail_report(String::new(), err),
    };
    let GameSource::GithubRelease {
        owner,
        repo,
        asset_contains,
        asset_pattern,
        include_prereleases,
    } = &game.source
    else {
        return fail_report(
            String::new(),
            format!("{} is played in the browser.", game.name),
        );
    };
    let install_dir = match resolve_install_dir(&loaded.settings, input.install_root, game) {
        Ok(path) => path,
        Err(err) => return fail_report(String::new(), err.to_string()),
    };
    let preserve = if game.preserve.is_empty() {
        vec!["Saved/SaveGames".into(), "Saved/Config".into()]
    } else {
        game.preserve.clone()
    };
    let token = saved_secret(input.github_token).map(str::to_string);
    let job = PatchJob {
        source: ReleaseSource {
            api_base: input.api_base.to_string(),
            owner: owner.clone(),
            repo: repo.clone(),
            asset_contains: asset_contains.clone(),
            asset_pattern: asset_pattern.clone(),
            include_prereleases: *include_prereleases,
            token,
        },
        install_dir,
        download_dir: input.data_dir.join("downloads").join(&game.id),
        preserve,
        game_id: game.id.clone(),
    };
    let mut lines = Vec::new();
    let manifest = match patch_game(client, &job, |progress| {
        lines.push(progress.message);
    })
    .await
    {
        Ok(manifest) => manifest,
        Err(err) => {
            let mut stdout = lines.join("\n");
            if !stdout.is_empty() {
                stdout.push('\n');
            }
            return fail_report(stdout, err.to_string());
        }
    };
    if let Err(err) = write_manifest(&manifest_path(input.data_dir), &manifest) {
        return fail_report(
            lines.join("\n"),
            format!("Could not record the installed client: {err}"),
        );
    }
    lines.push(format!("Client: {}", manifest.version));
    let mut report = ok_report(format!("{}\n", lines.join("\n")));
    report.versions.client = Some(manifest.version);
    report
}

async fn deploy(client: &reqwest::Client, input: &CliInput<'_>, version: &str) -> CliReport {
    let loaded = match load_context(input) {
        Ok(loaded) => loaded,
        Err(err) => return fail_report(String::new(), err),
    };
    let server = match pick_server(&loaded.settings, &loaded.servers) {
        Ok(server) => server,
        Err(err) => return fail_report(String::new(), err),
    };
    let mut stdout = format!("Deploying {version} to {}\n", server.name);
    let secret = match saved_secret(input.admin_key) {
        Some(secret) => secret,
        None => return fail_report(stdout, MISSING_ADMIN_KEY),
    };
    let base = match agent_base(server, input.agent_url_override) {
        Ok(base) => base,
        Err(err) => return fail_report(stdout, err),
    };
    let payload = match serde_json::to_vec(&serde_json::json!({ "tag": version })) {
        Ok(payload) => payload,
        Err(err) => return fail_report(stdout, err.to_string()),
    };
    let body = match agent_exchange(client, &base, secret, "POST", "/v1/deploy", &payload).await {
        Ok(body) => body,
        Err(err) => return fail_report(stdout, err.to_string()),
    };
    let action: crate::AgentAction = match serde_json::from_str(&body) {
        Ok(action) => action,
        Err(err) => {
            return fail_report(
                stdout,
                format!("Server agent returned an unexpected result: {err}"),
            )
        }
    };
    stdout.push_str(&action.message);
    if !stdout.ends_with('\n') {
        stdout.push('\n');
    }
    stdout.push_str(&format!("Server version: {}\n", action.status.version));
    stdout.push_str(&format!(
        "Server online: {}\n",
        yes_no(action.status.online)
    ));
    let mut report = if action.ok {
        ok_report(stdout)
    } else {
        fail_report(stdout, "Deploy did not succeed.")
    };
    report.versions.server = Some(action.status.version);
    report
}

struct Loaded {
    catalog: crate::Catalog,
    servers: Vec<ServerEntry>,
    settings: Settings,
}

fn load_context(input: &CliInput<'_>) -> Result<Loaded, String> {
    let catalog = load_catalog(input.games_json).map_err(|err| format!("games.json: {err}"))?;
    let map = load_servers(input.servers_json).map_err(|err| format!("servers.json: {err}"))?;
    let game = fnf_game(&catalog)?;
    let servers = map.get(&game.servers_key).cloned().unwrap_or_default();
    let settings_path = input.data_dir.join("settings.json");
    let settings = if settings_path.is_file() {
        load_settings(&settings_path)
            .map_err(|err| format!("Could not read settings.json: {err}"))?
    } else {
        Settings::default()
    };
    Ok(Loaded {
        catalog,
        servers,
        settings,
    })
}

fn fnf_game(catalog: &crate::Catalog) -> Result<&GameDef, String> {
    catalog
        .games
        .iter()
        .find(|game| game.id == FNF_GAME_ID)
        .ok_or_else(|| "Friday Night Fights is not in games.json.".to_string())
}

fn pick_server<'a>(
    settings: &Settings,
    servers: &'a [ServerEntry],
) -> Result<&'a ServerEntry, String> {
    if let Some(id) = settings.selected_server.get(FNF_GAME_ID) {
        if let Some(server) = servers.iter().find(|server| server.id == id.as_str()) {
            return Ok(server);
        }
    }
    servers
        .iter()
        .find(|server| server.default)
        .or_else(|| servers.first())
        .ok_or_else(|| "No servers are configured for this game.".to_string())
}

fn agent_base(server: &ServerEntry, override_url: Option<&str>) -> Result<String, String> {
    if let Some(url) = override_url.map(str::trim).filter(|url| !url.is_empty()) {
        return Ok(url.to_string());
    }
    let url = server.agent_url.trim();
    if url.is_empty() {
        return Err(format!(
            "{} has no server agent address. Add agentUrl in servers.json.",
            server.name
        ));
    }
    Ok(url.to_string())
}

fn saved_secret(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn manifest_path(data_dir: &Path) -> PathBuf {
    data_dir
        .join("manifests")
        .join(format!("{FNF_GAME_ID}.json"))
}

fn yes_no(online: bool) -> &'static str {
    if online {
        "yes"
    } else {
        "no"
    }
}

async fn agent_status(
    client: &reqwest::Client,
    base: &str,
    secret: &str,
) -> Result<crate::AgentStatus, String> {
    let body = agent_exchange(client, base, secret, "GET", "/v1/status", b"")
        .await
        .map_err(|err| err.to_string())?;
    serde_json::from_str(&body).map_err(|err| format!("Server agent status was unreadable: {err}"))
}

struct ProtectedFile {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
    modified: Option<SystemTime>,
}

struct ProtectedSnapshot {
    files: Vec<ProtectedFile>,
}

impl ProtectedSnapshot {
    fn capture(data_dir: &Path) -> Self {
        let files = PROTECTED_FILES
            .iter()
            .map(|name| {
                let path = data_dir.join(name);
                let bytes = fs::read(&path).ok();
                let modified = fs::metadata(&path).and_then(|meta| meta.modified()).ok();
                ProtectedFile {
                    path,
                    bytes,
                    modified,
                }
            })
            .collect();
        Self { files }
    }

    fn changed(&self) -> bool {
        self.files.iter().any(|file| match &file.bytes {
            Some(original) => match fs::read(&file.path) {
                Ok(current) if current == *original => {
                    let modified = fs::metadata(&file.path)
                        .and_then(|meta| meta.modified())
                        .ok();
                    modified != file.modified
                }
                _ => true,
            },
            None => file.path.exists(),
        })
    }

    fn restore(&self) {
        for file in &self.files {
            match &file.bytes {
                Some(bytes) => {
                    let _ = fs::write(&file.path, bytes);
                }
                None => {
                    let _ = fs::remove_file(&file.path);
                }
            }
        }
    }
}

impl Drop for ProtectedSnapshot {
    fn drop(&mut self) {
        if self.changed() {
            self.restore();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn arg(value: &str) -> String {
        value.to_string()
    }

    #[test]
    fn parses_one_command_and_rejects_mixtures() {
        assert_eq!(parse_cli(&[]).unwrap(), None);
        assert_eq!(parse_cli(&[arg("--help")]).unwrap(), Some(CliCommand::Help));
        assert_eq!(
            parse_cli(&[arg("--status"), arg("-h")]).unwrap(),
            Some(CliCommand::Help)
        );
        assert_eq!(
            parse_cli(&[arg("--version")]).unwrap(),
            Some(CliCommand::Version)
        );
        assert_eq!(
            parse_cli(&[arg("--status")]).unwrap(),
            Some(CliCommand::Status)
        );
        assert_eq!(
            parse_cli(&[arg("--update-client")]).unwrap(),
            Some(CliCommand::UpdateClient)
        );
        assert_eq!(
            parse_cli(&[arg("--deploy-server"), arg("v1.2.0")]).unwrap(),
            Some(CliCommand::DeployServer {
                version: "v1.2.0".into()
            })
        );
        assert!(parse_cli(&[arg("--deploy-server")]).is_err());
        assert!(parse_cli(&[arg("--nope")]).is_err());
        assert!(parse_cli(&[arg("--status"), arg("--update-client")]).is_err());
        let help = help_text("0.3.1");
        assert!(help.contains("%LOCALAPPDATA%\\uXa\\Launcher\\servers.json"));
        assert!(help.contains("--deploy-server"));
        assert!(help.contains("admin key"));
    }

    #[test]
    fn redact_cli_strips_the_key_and_token() {
        let text = "failed ghp_cliTestTokenValue123 and dolmar-admin-key-9f3a";
        let out = redact_cli(text, &["dolmar-admin-key-9f3a", "ghp_cliTestTokenValue123"]);
        assert!(!out.contains("dolmar-admin"));
        assert!(!out.contains("ghp_"));
        assert!(out.contains("[redacted]"));
    }

    #[test]
    fn snapshot_restores_settings_key_and_token() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("settings.json"), b"original-settings").unwrap();
        fs::write(dir.path().join("servers.json"), b"original-servers").unwrap();
        fs::write(dir.path().join("agent-secret"), b"original-key").unwrap();
        let snap = ProtectedSnapshot::capture(dir.path());
        fs::write(dir.path().join("settings.json"), b"rewritten").unwrap();
        fs::remove_file(dir.path().join("agent-secret")).unwrap();
        fs::write(dir.path().join("github-token"), b"ghp_new").unwrap();
        assert!(snap.changed());
        snap.restore();
        assert_eq!(
            fs::read(dir.path().join("settings.json")).unwrap(),
            b"original-settings"
        );
        assert_eq!(
            fs::read(dir.path().join("servers.json")).unwrap(),
            b"original-servers"
        );
        assert_eq!(
            fs::read(dir.path().join("agent-secret")).unwrap(),
            b"original-key"
        );
        assert!(!dir.path().join("github-token").exists());
        // restore() writes the original bytes back, which updates mtime, so a
        // second changed() check would still be true. The bytes above are the check.
    }

    struct Mock {
        zip: Vec<u8>,
        deploy_tags: Vec<String>,
        status_hits: u32,
        deploy_hits: u32,
    }

    fn zip_bytes() -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let opts = zip::write::SimpleFileOptions::default();
            writer.start_file("FridayNightFights.exe", opts).unwrap();
            writer.write_all(b"MZ-client").unwrap();
            writer.finish().unwrap();
        }
        cursor.into_inner()
    }

    fn start_mock() -> (String, Arc<Mutex<Mock>>) {
        let state = Arc::new(Mutex::new(Mock {
            zip: zip_bytes(),
            deploy_tags: Vec::new(),
            status_hits: 0,
            deploy_hits: 0,
        }));
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let shared = Arc::clone(&state);
        thread::spawn(move || {
            for request in server.incoming_requests() {
                if let Err(err) = respond(&shared, request) {
                    eprintln!("mock cli server: {err}");
                }
            }
        });
        (format!("http://127.0.0.1:{port}"), state)
    }

    fn respond(
        state: &Mutex<Mock>,
        mut request: tiny_http::Request,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let url = request.url().to_string();
        if url.contains("/releases") {
            let size = state.lock().unwrap().zip.len();
            let port = request
                .headers()
                .iter()
                .find(|header| header.field.equiv("Host"))
                .map(|header| header.value.as_str().to_string())
                .unwrap_or_else(|| "127.0.0.1".into());
            let body = serde_json::json!([{
                "tag_name": "v1.2.0",
                "name": "v1.2.0",
                "body": "",
                "draft": false,
                "prerelease": false,
                "assets": [{
                    "name": "FridayNightFights_Client_Win64.zip",
                    "url": format!("http://{port}/file.zip"),
                    "browser_download_url": format!("http://{port}/file.zip"),
                    "size": size,
                    "digest": null
                }]
            }])
            .to_string();
            let header =
                tiny_http::Header::from_bytes(b"Content-Type", b"application/json").unwrap();
            request
                .respond(tiny_http::Response::from_data(body.into_bytes()).with_header(header))?;
            return Ok(());
        }
        if url.contains("/file.zip") {
            let zip = state.lock().unwrap().zip.clone();
            request.respond(tiny_http::Response::from_data(zip))?;
            return Ok(());
        }
        if url.starts_with("/v1/status") {
            state.lock().unwrap().status_hits += 1;
            let body = serde_json::json!({
                "version": "v8.0.0",
                "online": true,
                "uptimeSecs": 4,
                "playerCount": 1,
                "port": 7777,
                "phase": "running"
            })
            .to_string();
            request.respond(tiny_http::Response::from_data(body.into_bytes()))?;
            return Ok(());
        }
        if url.starts_with("/v1/deploy") {
            let mut raw = String::new();
            request.as_reader().read_to_string(&mut raw)?;
            let tag = serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|value| {
                    value
                        .get("tag")
                        .and_then(|tag| tag.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let mut mock = state.lock().unwrap();
            mock.deploy_hits += 1;
            mock.deploy_tags.push(tag.clone());
            let body = serde_json::json!({
                "ok": true,
                "message": format!("Deployed {tag}."),
                "status": {
                    "version": tag,
                    "online": true,
                    "uptimeSecs": 1,
                    "playerCount": 0,
                    "port": 7777,
                    "phase": "running"
                }
            })
            .to_string();
            request.respond(tiny_http::Response::from_data(body.into_bytes()))?;
            return Ok(());
        }
        request.respond(tiny_http::Response::from_data(Vec::<u8>::new()).with_status_code(404))?;
        Ok(())
    }

    fn games_json() -> String {
        include_str!("../../../config/games.json").to_string()
    }

    fn servers_at(agent_url: &str) -> String {
        serde_json::json!({
            "friday-night-fights": [{
                "id": "dolmar",
                "name": "Dolmar's Server",
                "host": "71.71.45.128",
                "port": 7777,
                "default": true,
                "agentUrl": agent_url
            }, {
                "id": "localhost",
                "name": "Localhost",
                "host": "127.0.0.1",
                "port": 7777,
                "default": false,
                "agentUrl": ""
            }]
        })
        .to_string()
    }

    struct Fixture {
        data: tempfile::TempDir,
        games: tempfile::TempDir,
        settings: Vec<u8>,
        servers: Vec<u8>,
        key: Vec<u8>,
        token: Vec<u8>,
    }

    fn fixture(agent_url: &str) -> Fixture {
        let data = tempfile::tempdir().unwrap();
        let games = tempfile::tempdir().unwrap();
        let settings = br#"{"checkOnStart":false,"selectedServer":{}}"#.to_vec();
        let servers = servers_at(agent_url).into_bytes();
        let key = b"dolmar-admin-key-9f3a".to_vec();
        let token = b"ghp_cliTestTokenValue123".to_vec();
        fs::write(data.path().join("settings.json"), &settings).unwrap();
        fs::write(data.path().join("servers.json"), &servers).unwrap();
        fs::write(data.path().join("agent-secret"), &key).unwrap();
        fs::write(data.path().join("github-token"), &token).unwrap();
        Fixture {
            data,
            games,
            settings,
            servers,
            key,
            token,
        }
    }

    fn cli_input<'a>(
        fixture: &'a Fixture,
        games: &'a str,
        servers: &'a str,
        api_base: &'a str,
        token: &'a str,
        agent_url_override: Option<&'a str>,
        admin_key: Option<&'a str>,
    ) -> CliInput<'a> {
        CliInput {
            launcher_version: "0.3.1",
            data_dir: fixture.data.path(),
            install_root: fixture.games.path(),
            games_json: games,
            servers_json: servers,
            api_base,
            agent_url_override,
            admin_key,
            github_token: Some(token),
        }
    }

    fn assert_untouched(fixture: &Fixture, report: &CliReport) {
        assert_eq!(
            fs::read(fixture.data.path().join("settings.json")).unwrap(),
            fixture.settings
        );
        assert_eq!(
            fs::read(fixture.data.path().join("servers.json")).unwrap(),
            fixture.servers
        );
        assert_eq!(
            fs::read(fixture.data.path().join("agent-secret")).unwrap(),
            fixture.key
        );
        assert_eq!(
            fs::read(fixture.data.path().join("github-token")).unwrap(),
            fixture.token
        );
        let combined = format!("{}{}", report.stdout, report.stderr);
        assert!(!combined.contains("dolmar-admin-key-9f3a"));
        assert!(!combined.contains("ghp_"));
        assert!(!combined.contains("cliTestTokenValue123"));
    }

    #[tokio::test]
    async fn commands_use_the_override_and_leave_secrets_in_place() {
        let (base, state) = start_mock();
        let fixture = fixture(&base);
        let games = games_json();
        let servers = servers_at(&base);
        let key = std::str::from_utf8(&fixture.key).unwrap();
        let token = std::str::from_utf8(&fixture.token).unwrap();
        let client = crate::http_client().unwrap();

        let status_report = execute_cli(
            &client,
            &cli_input(&fixture, &games, &servers, &base, token, None, Some(key)),
            CliCommand::Status,
        )
        .await;
        assert_eq!(status_report.exit_code, EXIT_OK, "{}", status_report.stderr);
        assert!(status_report.stdout.contains("Launcher: 0.3.1"));
        assert!(status_report.stdout.contains("Client: not installed"));
        assert!(status_report.stdout.contains("Server: Dolmar's Server"));
        assert!(status_report.stdout.contains("Server version: v8.0.0"));
        assert!(status_report.stdout.contains("Server online: yes"));
        assert_eq!(status_report.versions.launcher, "0.3.1");
        assert_eq!(status_report.versions.client, None);
        assert_eq!(status_report.versions.server.as_deref(), Some("v8.0.0"));
        assert_untouched(&fixture, &status_report);

        let deploy_report = execute_cli(
            &client,
            &cli_input(&fixture, &games, &servers, &base, token, None, Some(key)),
            CliCommand::DeployServer {
                version: "v9.1.0".into(),
            },
        )
        .await;
        assert_eq!(deploy_report.exit_code, EXIT_OK, "{}", deploy_report.stderr);
        assert!(deploy_report
            .stdout
            .contains("Deploying v9.1.0 to Dolmar's Server"));
        assert!(deploy_report.stdout.contains("Deployed v9.1.0."));
        assert!(deploy_report.stdout.contains("Server version: v9.1.0"));
        assert!(deploy_report.stdout.contains("Server online: yes"));
        assert_untouched(&fixture, &deploy_report);
        assert_eq!(
            state.lock().unwrap().deploy_tags,
            vec!["v9.1.0".to_string()]
        );
        assert!(state.lock().unwrap().status_hits >= 1);
        assert_eq!(state.lock().unwrap().deploy_hits, 1);

        let update_report = execute_cli(
            &client,
            &cli_input(&fixture, &games, &servers, &base, token, None, Some(key)),
            CliCommand::UpdateClient,
        )
        .await;
        assert_eq!(update_report.exit_code, EXIT_OK, "{}", update_report.stderr);
        assert!(update_report.stdout.contains("Client: v1.2.0"));
        assert!(!update_report.stdout.to_ascii_lowercase().contains("launch"));
        assert_untouched(&fixture, &update_report);
        let manifest = read_manifest(&manifest_path(fixture.data.path()))
            .unwrap()
            .unwrap();
        assert_eq!(manifest.version, "v1.2.0");

        let bundled = include_str!("../../../config/servers.json").to_string();
        let override_input = CliInput {
            launcher_version: "0.3.1",
            data_dir: fixture.data.path(),
            install_root: fixture.games.path(),
            games_json: &games,
            servers_json: &bundled,
            api_base: &base,
            agent_url_override: Some(base.as_str()),
            admin_key: Some(key),
            github_token: Some(token),
        };
        let before = state.lock().unwrap().status_hits;
        let overridden = execute_cli(&client, &override_input, CliCommand::Status).await;
        assert_eq!(overridden.exit_code, EXIT_OK, "{}", overridden.stderr);
        assert!(state.lock().unwrap().status_hits > before);
        assert!(!overridden.stdout.contains("71.71.45.128"));
        assert_untouched(&fixture, &overridden);
    }

    #[tokio::test]
    async fn missing_admin_key_fails_without_writing_settings() {
        let fixture = fixture("http://127.0.0.1:9");
        let games = games_json();
        let servers = servers_at("http://127.0.0.1:9");
        let token = std::str::from_utf8(&fixture.token).unwrap();
        let client = crate::http_client().unwrap();
        let input = CliInput {
            launcher_version: "0.3.1",
            data_dir: fixture.data.path(),
            install_root: fixture.games.path(),
            games_json: &games,
            servers_json: &servers,
            api_base: "http://127.0.0.1:9",
            agent_url_override: None,
            admin_key: None,
            github_token: Some(token),
        };
        let report = execute_cli(
            &client,
            &input,
            CliCommand::DeployServer {
                version: "v9.1.0".into(),
            },
        )
        .await;
        assert_eq!(report.exit_code, EXIT_FAILURE);
        assert!(report.stderr.contains("Unlock admin once"));
        assert!(!report.stderr.contains("127.0.0.1"));
        assert_untouched(&fixture, &report);

        let status_report = execute_cli(&client, &input, CliCommand::Status).await;
        assert_eq!(status_report.exit_code, EXIT_FAILURE);
        assert!(status_report.stdout.contains("Launcher: 0.3.1"));
        assert!(status_report.stderr.contains(MISSING_ADMIN_KEY));
        assert_untouched(&fixture, &status_report);
    }

    #[tokio::test]
    async fn selected_server_is_used_instead_of_the_default() {
        let (base, state) = start_mock();
        let fixture = fixture(&base);
        let settings = format!(
            r#"{{"checkOnStart":false,"selectedServer":{{"friday-night-fights":"localhost"}}}}"#
        );
        fs::write(fixture.data.path().join("settings.json"), &settings).unwrap();
        let games = games_json();
        let servers = servers_at(&base);
        let key = std::str::from_utf8(&fixture.key).unwrap();
        let token = std::str::from_utf8(&fixture.token).unwrap();
        let client = crate::http_client().unwrap();
        let report = execute_cli(
            &client,
            &cli_input(&fixture, &games, &servers, &base, token, None, Some(key)),
            CliCommand::Status,
        )
        .await;
        assert_eq!(report.exit_code, EXIT_FAILURE);
        assert!(report.stdout.contains("Server: Localhost"));
        assert!(report.stderr.contains("no server agent address"));
        assert_eq!(state.lock().unwrap().status_hits, 0);
        assert_eq!(
            fs::read_to_string(fixture.data.path().join("settings.json")).unwrap(),
            settings
        );
        assert_eq!(
            fs::read(fixture.data.path().join("agent-secret")).unwrap(),
            fixture.key
        );
    }

    #[test]
    fn version_and_help_do_not_need_a_saved_key() {
        let dir = tempfile::tempdir().unwrap();
        let games = games_json();
        let input = CliInput {
            launcher_version: "0.3.1",
            data_dir: dir.path(),
            install_root: dir.path(),
            games_json: &games,
            servers_json: "{}",
            api_base: "http://127.0.0.1:9",
            agent_url_override: None,
            admin_key: None,
            github_token: None,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let client = crate::http_client().unwrap();
        let version = runtime.block_on(execute_cli(&client, &input, CliCommand::Version));
        assert_eq!(version.exit_code, EXIT_OK);
        assert_eq!(version.stdout, "uXa Launcher 0.3.1\n");
        assert_eq!(version.versions.launcher, "0.3.1");
        assert_eq!(version.versions.client, None);
        assert_eq!(version.versions.server, None);
        assert!(!dir.path().join("settings.json").exists());
        assert!(!dir.path().join("agent-secret").exists());
    }

    #[test]
    fn redirected_handles_are_not_replaced_by_a_console() {
        let keep = stdio_plan(InheritedHandle::Redirected, InheritedHandle::Redirected);
        assert!(!keep.attach_console);
        assert!(!keep.replace_stdout);
        assert!(!keep.replace_stderr);

        let console = stdio_plan(InheritedHandle::Console, InheritedHandle::Console);
        assert!(!console.attach_console);

        let gui = stdio_plan(InheritedHandle::Missing, InheritedHandle::Missing);
        assert!(gui.attach_console);
        assert!(gui.replace_stdout);
        assert!(gui.replace_stderr);

        // `program > file` redirects stdout only. The pipe must stay the pipe.
        let stdout_file = stdio_plan(InheritedHandle::Redirected, InheritedHandle::Missing);
        assert!(stdout_file.attach_console);
        assert!(!stdout_file.replace_stdout);
        assert!(stdout_file.replace_stderr);
    }

    #[test]
    fn cli_result_file_records_success_failure_and_usage() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cli-last.json");

        let mut version = ok_report("uXa Launcher 0.3.3\n".into());
        version.versions.launcher = "0.3.3".into();
        let record = cli_result_record("--version", &version);
        write_cli_result(&path, &record).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["command"], "--version");
        assert_eq!(saved["ok"], true);
        assert_eq!(saved["exitCode"], 0);
        assert_eq!(saved["message"], "uXa Launcher 0.3.3");
        assert_eq!(saved["versions"]["launcher"], "0.3.3");
        assert!(saved["versions"]["client"].is_null());
        assert!(saved["versions"]["server"].is_null());
        let stamp = saved["timestamp"].as_str().unwrap();
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert!(stamp.contains('T'), "{stamp}");

        let mut failed = fail_report(
            "Launcher: 0.3.3\n".into(),
            "No admin key is saved. token ghp_cliTestTokenValue123\n",
        );
        failed.versions.launcher = "0.3.3".into();
        failed.versions.client = Some("v0.9.7.2".into());
        write_cli_result(&path, &cli_result_record("--status", &failed)).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["command"], "--status");
        assert_eq!(saved["ok"], false);
        assert_eq!(saved["exitCode"], 1);
        assert_eq!(saved["versions"]["client"], "v0.9.7.2");
        assert!(saved["versions"]["server"].is_null());
        let message = saved["message"].as_str().unwrap();
        assert!(message.contains("No admin key"));
        assert!(!message.contains("ghp_"));
        assert!(message.contains("[redacted]"));

        let usage = usage_cli_result("0.3.3", "--nope", "Unknown argument --nope.");
        assert!(!usage.ok);
        assert_eq!(usage.exit_code, EXIT_USAGE);
        write_cli_result(&path, &usage).unwrap();
        let saved: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["command"], "--nope");
        assert_eq!(saved["ok"], false);
        assert_eq!(saved["exitCode"], 2);
        assert_eq!(saved["versions"]["launcher"], "0.3.3");
        assert!(!dir.path().join("settings.json").exists());
    }
}
