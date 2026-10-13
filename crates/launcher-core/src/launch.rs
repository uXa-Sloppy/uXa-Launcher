use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::{Error, Result, ServerEntry};

pub enum PlayPlan {
    Process {
        program: PathBuf,
        args: Vec<String>,
        working_dir: PathBuf,
    },
}

pub struct ClientLaunch<'a> {
    pub install_dir: &'a Path,
    pub executable_name: &'a str,
    pub servers_config_relative: &'a str,
    pub servers: &'a [ServerEntry],
    pub selected: &'a ServerEntry,
}

pub fn validate_server(server: &ServerEntry) -> Result<()> {
    if server.host.is_empty()
        || !server
            .host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
    {
        return Err(Error::msg(format!(
            "Server host {:?} is not a plain hostname or IP address.",
            server.host
        )));
    }
    if server.port == 0 {
        return Err(Error::msg(format!("{} has an empty port.", server.name)));
    }
    Ok(())
}

pub fn format_address(server: &ServerEntry) -> String {
    format!("{}:{}", server.host, server.port)
}

pub fn format_servers_ini(servers: &[ServerEntry]) -> String {
    let mut out = String::new();
    for server in servers {
        let name = server.name.replace(['|', '\n', '\r'], " ");
        let host = server
            .host
            .replace(['|', '\n', '\r', ' '], "");
        out.push_str(&format!("{name} | {host} | {}\n", server.port));
    }
    out
}

fn relative_inside(root: &Path, relative: &str) -> Result<PathBuf> {
    let rel = Path::new(relative);
    if relative.trim().is_empty() || rel.is_absolute() {
        return Err(Error::msg(format!(
            "Refusing servers config path {relative:?}."
        )));
    }
    if rel.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(Error::msg(format!(
            "Refusing servers config path {relative:?}."
        )));
    }
    Ok(root.join(rel))
}

pub fn write_servers_ini(
    install_dir: &Path,
    relative: &str,
    servers: &[ServerEntry],
) -> Result<PathBuf> {
    for server in servers {
        validate_server(server)?;
    }
    let path = relative_inside(install_dir, relative)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::File::create(&path)?;
    file.write_all(format_servers_ini(servers).as_bytes())?;
    file.sync_all()?;
    Ok(path)
}

pub fn find_executable(install_dir: &Path, name: &str) -> Result<PathBuf> {
    if name.trim().is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return Err(Error::msg(format!("Refusing executable name {name:?}.")));
    }
    let direct = install_dir.join(name);
    if direct.is_file() {
        return Ok(direct);
    }
    let mut found = None;
    let mut walked = 0usize;
    walk_for_exe(install_dir, name, 0, &mut walked, &mut found)?;
    found.ok_or_else(|| {
        Error::msg(format!(
            "Could not find {name} under {}.",
            install_dir.display()
        ))
    })
}

fn walk_for_exe(
    dir: &Path,
    name: &str,
    depth: usize,
    walked: &mut usize,
    found: &mut Option<PathBuf>,
) -> Result<()> {
    if found.is_some() || depth > 8 || *walked > 8_000 {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        if found.is_some() {
            break;
        }
        *walked += 1;
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name == name && path.is_file() {
            *found = Some(path);
            return Ok(());
        }
        if path.is_dir() && file_name != "Saved" && !file_name.starts_with('.') {
            walk_for_exe(&path, name, depth + 1, walked, found)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = fs::metadata(path)?;
    let mode = meta.permissions().mode();
    if mode & 0o111 == 0 {
        let mut permissions = meta.permissions();
        permissions.set_mode(mode | 0o755);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// Write `Config/Servers.ini` and build the Unreal command: the server address is the first argument.
pub fn prepare_client_launch(spec: ClientLaunch<'_>) -> Result<PlayPlan> {
    validate_server(spec.selected)?;
    write_servers_ini(spec.install_dir, spec.servers_config_relative, spec.servers)?;
    let program = find_executable(spec.install_dir, spec.executable_name)?;
    ensure_executable(&program)?;
    let working_dir = program
        .parent()
        .unwrap_or(spec.install_dir)
        .to_path_buf();
    Ok(PlayPlan::Process {
        program,
        args: vec![format_address(spec.selected)],
        working_dir,
    })
}

pub fn spawn_process(plan: &PlayPlan) -> Result<u32> {
    let PlayPlan::Process {
        program,
        args,
        working_dir,
    } = plan;
    tracing::info!(
        "launch {} {} (cwd {})",
        program.display(),
        args.join(" "),
        working_dir.display()
    );
    let child = Command::new(program)
        .args(args)
        .current_dir(working_dir)
        .spawn()?;
    Ok(child.id())
}

/// Best-effort reachability. Game servers speak UDP, so a TCP timeout is reported as `unknown`.
pub async fn probe_tcp(host: &str, port: u16) -> &'static str {
    if host.is_empty() || port == 0 {
        return "unknown";
    }
    match tokio::time::timeout(
        Duration::from_millis(800),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    {
        Ok(Ok(_)) => "online",
        Ok(Err(err)) if err.kind() == std::io::ErrorKind::ConnectionRefused => "unreachable",
        _ => "unknown",
    }
}
