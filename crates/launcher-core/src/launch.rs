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
        args: vec![format_address(spec.selected), account_server_arg(spec.selected)],
        working_dir,
    })
}

/// Path on the server agent that proxies to its local account server (0.3.10+).
pub const ACCOUNT_PROXY_PATH: &str = "/fnf-accounts";

/// `-FNFAccountServer=<spec>`. The client sends invite sign-ups there. Passing it
/// explicitly also overrides the client's built-in "never call Dolmar's IP" test guard.
///
/// From 0.3.10 sign-ups go through the server agent (its tunnel), not a forwarded TCP 7798:
/// `accountUrl`, else `agentUrl` + `/fnf-accounts`. Game clients up to 0.10.0 only take
/// `host:port` and build `http://host:port/v1/signup`, so a URL with a path is passed as
/// `host:port/path:port`; the client then requests `http://host:port/path:port/v1/signup`,
/// which the agent accepts. Falls back to the legacy `host:accountPort|7798`.
pub fn account_server_arg(server: &ServerEntry) -> String {
    let from_url = if !server.account_url.trim().is_empty() {
        account_spec_from_url(server.account_url.trim())
    } else {
        agent_account_url(&server.agent_url).and_then(|url| account_spec_from_url(&url))
    };
    match from_url {
        Some(spec) => format!("-FNFAccountServer={spec}"),
        None => format!(
            "-FNFAccountServer={}:{}",
            server.host.trim(),
            server.account_port.filter(|p| *p != 0).unwrap_or(7798)
        ),
    }
}

/// Public sign-up base URL behind an agent URL, when a game client can reach it over plain
/// HTTP: an `http://` agent, or an `https://` agent on the default port (a Cloudflare tunnel
/// or other proxy, which also answers plain HTTP on port 80). A direct TLS listener such as
/// `https://1.2.3.4:8788` cannot be used by the game client, so that returns None.
pub fn agent_account_url(agent_url: &str) -> Option<String> {
    let url = agent_url.trim().trim_end_matches('/');
    if url.is_empty() {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split('/').next().unwrap_or("");
    let explicit_port = authority.rsplit_once(':').map(|(_, p)| p).filter(|p| p.chars().all(|c| c.is_ascii_digit()));
    match scheme.to_ascii_lowercase().as_str() {
        "http" => Some(format!("http://{authority}{ACCOUNT_PROXY_PATH}")),
        "https" if explicit_port.is_none() || explicit_port == Some("443") => {
            let host = authority.rsplit_once(':').map(|(h, _)| h).filter(|_| explicit_port.is_some()).unwrap_or(authority);
            Some(format!("http://{host}{ACCOUNT_PROXY_PATH}"))
        }
        _ => None,
    }
}

/// Turn `http(s)://host[:port][/path]` into the `-FNFAccountServer` value a 0.10.0 client can use.
pub fn account_spec_from_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, p.parse::<u16>().ok()?),
        _ => (authority, 80u16),
    };
    // The client speaks plain HTTP only: an https URL on 443 is reached on port 80 (tunnels/proxies answer both).
    let port = if scheme == "https" && port == 443 { 80 } else { port };
    if host.is_empty()
        || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        || matches!(port, 0 | 7777 | 7797)
        || !path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        return None;
    }
    if path.is_empty() {
        Some(format!("{host}:{port}"))
    } else {
        Some(format!("{host}:{port}{path}:{port}"))
    }
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

#[cfg(test)]
mod account_arg_tests {
    use super::*;

    fn entry(agent_url: &str, account_url: &str, account_port: Option<u16>) -> ServerEntry {
        ServerEntry {
            id: "dolmar".into(),
            name: "Dolmar's Server".into(),
            host: "71.71.45.128".into(),
            port: 7777,
            default: true,
            agent_url: agent_url.into(),
            account_port,
            account_url: account_url.into(),
        }
    }

    #[test]
    fn tunnel_agent_url_goes_through_the_agent_proxy() {
        let s = entry("https://seats-fashion-ware-origin.trycloudflare.com", "", Some(7798));
        assert_eq!(
            account_server_arg(&s),
            "-FNFAccountServer=seats-fashion-ware-origin.trycloudflare.com:80/fnf-accounts:80"
        );
    }

    #[test]
    fn http_agent_keeps_its_port() {
        let s = entry("http://10.0.0.5:8788/", "", None);
        assert_eq!(account_server_arg(&s), "-FNFAccountServer=10.0.0.5:8788/fnf-accounts:8788");
    }

    #[test]
    fn explicit_account_url_wins() {
        let s = entry("https://a.trycloudflare.com", "https://b.example.org/fnf-accounts", None);
        assert_eq!(account_server_arg(&s), "-FNFAccountServer=b.example.org:80/fnf-accounts:80");
        let s = entry("", "http://127.0.0.1:7798", None);
        assert_eq!(account_server_arg(&s), "-FNFAccountServer=127.0.0.1:7798");
    }

    #[test]
    fn direct_tls_agent_falls_back_to_legacy_port() {
        let s = entry("https://71.71.45.128:8788", "", None);
        assert_eq!(account_server_arg(&s), "-FNFAccountServer=71.71.45.128:7798");
        let s = entry("", "", Some(7900));
        assert_eq!(account_server_arg(&s), "-FNFAccountServer=71.71.45.128:7900");
    }

    #[test]
    fn rejects_game_ports_and_odd_urls() {
        assert_eq!(account_spec_from_url("http://h:7777/x"), None);
        assert_eq!(account_spec_from_url("ftp://h/x"), None);
        assert_eq!(account_spec_from_url("http://h/a b"), None);
        assert_eq!(agent_account_url("https://h:443"), Some("http://h/fnf-accounts".into()));
    }
}