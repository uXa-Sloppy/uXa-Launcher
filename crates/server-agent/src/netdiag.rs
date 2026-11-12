//! Read-only network snapshot for `GET /v1/diag/net`.
//!
//! Nothing here changes the firewall, the router, the service, the game, or
//! any file. Command output is parsed in process. Addresses are left intact
//! so the operator can see them; `key=` / `token=` values are still redacted.
#![cfg_attr(not(any(windows, test)), allow(dead_code))]

use std::fs;
#[cfg(windows)]
use std::io::Read;
use std::path::Path;
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use serde_json::{json, Value};

use crate::diag::{ipv4_at, redact_secrets_only};
use crate::runtime::Agent;

#[cfg(windows)]
const CMD_TIMEOUT: Duration = Duration::from_secs(8);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(windows)]
const MAX_CMD_BYTES: usize = 4 * 1024 * 1024;
const MAX_RULES: usize = 80;
const MAX_WALK: usize = 4000;
const MAX_CHILDREN: usize = 32;

const IP_SOURCES: &[(&str, &str)] = &[
    ("api.ipify.org", "https://api.ipify.org"),
    ("ifconfig.me", "https://ifconfig.me/ip"),
    ("icanhazip.com", "https://icanhazip.com"),
];

/// PowerShell is read-only: Get-NetUDPEndpoint, Get-NetIPAddress, Get-NetRoute,
/// Get-NetConnectionProfile, and Win32_Process. Firewall rules come from
/// `netsh advfirewall firewall show rule name=all verbose` because walking
/// every rule through Get-NetFirewallPortFilter does not fit in the time bound.
#[cfg(windows)]
const HOST_SCRIPT: &str = r#"
$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
$port = 7777
if ($env:UXA_DIAG_PORT -match '^\d{1,5}$') {
  $n = [int]$env:UXA_DIAG_PORT
  if ($n -ge 1 -and $n -le 65535) { $port = $n }
}
$listeningError = $null
$endpoints = @()
$lanError = $null
$addresses = @()
$gateways = @()
$profiles = @()
$processesError = $null
$processes = @()
try {
  $eps = @(Get-NetUDPEndpoint -LocalPort $port -ErrorAction Stop)
  $list = New-Object System.Collections.Generic.List[object]
  foreach ($ep in $eps) {
    $name = ''
    $exe = ''
    $own = 0
    try { $own = [int]$ep.OwningProcess } catch { $own = 0 }
    if ($own -gt 0) {
      try {
        $proc = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId=$own" -ErrorAction Stop
        if ($null -ne $proc) {
          $name = [string]$proc.Name
          $exe = [string]$proc.ExecutablePath
        }
      } catch {}
    }
    $list.Add([pscustomobject]@{
      localAddress = [string]$ep.LocalAddress
      localPort = [int]$ep.LocalPort
      owningProcess = $own
      processName = $name
      processPath = $exe
    }) | Out-Null
  }
  if ($list.Count -gt 0) { $endpoints = $list.ToArray() }
} catch {
  $listeningError = [string]$_.Exception.Message
}
try {
  $addrList = New-Object System.Collections.Generic.List[object]
  foreach ($a in @(Get-NetIPAddress -AddressFamily IPv4 -ErrorAction Stop)) {
    $addrList.Add([pscustomobject]@{
      interfaceAlias = [string]$a.InterfaceAlias
      ipAddress = [string]$a.IPAddress
      prefixLength = [int]$a.PrefixLength
    }) | Out-Null
  }
  if ($addrList.Count -gt 0) { $addresses = $addrList.ToArray() }
  $gwList = New-Object System.Collections.Generic.List[object]
  foreach ($g in @(Get-NetRoute -DestinationPrefix '0.0.0.0/0' -AddressFamily IPv4 -ErrorAction SilentlyContinue)) {
    $metric = 0
    try { $metric = [int]$g.RouteMetric } catch { $metric = 0 }
    $gwList.Add([pscustomobject]@{
      interfaceAlias = [string]$g.InterfaceAlias
      nextHop = [string]$g.NextHop
      routeMetric = $metric
    }) | Out-Null
  }
  if ($gwList.Count -gt 0) { $gateways = $gwList.ToArray() }
  $catList = New-Object System.Collections.Generic.List[object]
  foreach ($c in @(Get-NetConnectionProfile -ErrorAction SilentlyContinue)) {
    $catList.Add([pscustomobject]@{
      name = [string]$c.Name
      interfaceAlias = [string]$c.InterfaceAlias
      networkCategory = [string]$c.NetworkCategory
    }) | Out-Null
  }
  if ($catList.Count -gt 0) { $profiles = $catList.ToArray() }
} catch {
  $lanError = [string]$_.Exception.Message
}
try {
  $procList = New-Object System.Collections.Generic.List[object]
  foreach ($p in @(Get-CimInstance -ClassName Win32_Process -ErrorAction Stop)) {
    $pidN = 0
    $ppid = 0
    try { $pidN = [int]$p.ProcessId } catch { $pidN = 0 }
    try { $ppid = [int]$p.ParentProcessId } catch { $ppid = 0 }
    $procList.Add([pscustomobject]@{
      processId = $pidN
      parentProcessId = $ppid
      name = [string]$p.Name
      executablePath = [string]$p.ExecutablePath
    }) | Out-Null
  }
  if ($procList.Count -gt 0) { $processes = $procList.ToArray() }
} catch {
  $processesError = [string]$_.Exception.Message
}
[pscustomobject]@{
  listeningError = $listeningError
  endpoints = $endpoints
  lanError = $lanError
  addresses = $addresses
  gateways = $gateways
  profiles = $profiles
  processesError = $processesError
  processes = $processes
} | ConvertTo-Json -Depth 5 -Compress
"#;

pub fn parse_diag_port(raw: &str, game_port: u16) -> Result<u16, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(7777);
    }
    let port: u16 = raw
        .parse()
        .map_err(|_| "port must be a number".to_string())?;
    if port == 7777 || port == game_port {
        Ok(port)
    } else {
        Err("port must be the game port or 7777".into())
    }
}

pub fn parse_exact_ipv4(text: &str) -> Option<String> {
    let text = text.trim();
    let len = ipv4_at(text, 0)?;
    if len == text.len() {
        Some(text.to_string())
    } else {
        None
    }
}

fn parse_public_ipv4(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(len) = ipv4_at(text, i) {
            return Some(text[i..i + len].to_string());
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone)]
struct ServerExe {
    role: &'static str,
    path: String,
    file_name: String,
    exists: bool,
}

#[derive(Debug, Clone)]
struct RawRule {
    name: String,
    enabled: bool,
    direction: String,
    action: String,
    profile: String,
    protocol: String,
    local_port: String,
    program: String,
}

#[derive(Debug, Clone)]
struct ProfileInfo {
    name: String,
    enabled: bool,
    default_inbound: String,
}

#[derive(Debug, Clone)]
struct Endpoint {
    local_address: String,
    local_port: u16,
    pid: u32,
    process_name: String,
    process_path: String,
}

#[derive(Debug, Clone)]
struct ProcInfo {
    pid: u32,
    ppid: u32,
    name: String,
    path: String,
}

#[derive(Debug, Clone)]
struct LanAddr {
    adapter: String,
    address: String,
    prefix: u32,
}

#[derive(Debug, Clone)]
struct Gateway {
    adapter: String,
    hop: String,
    metric: u32,
}

#[derive(Debug, Clone)]
struct ConnProfile {
    name: String,
    adapter: String,
    category: String,
}

struct HostSnapshot {
    listening_error: Option<String>,
    endpoints: Vec<Endpoint>,
    lan_error: Option<String>,
    addresses: Vec<LanAddr>,
    gateways: Vec<Gateway>,
    profiles: Vec<ConnProfile>,
    processes_error: Option<String>,
    processes: Vec<ProcInfo>,
}

struct LocalRaw {
    firewall_error: Option<String>,
    rules: Vec<RawRule>,
    profiles: Vec<ProfileInfo>,
    host: HostSnapshot,
    tracked_exe: Option<String>,
    tracked_children: Vec<ProcInfo>,
}

pub async fn collect(agent: &Agent, port: u16, expect: Option<String>) -> Value {
    let install = agent.config.install_dir.clone();
    let bootstrap = agent.config.executable.clone();
    let tracked = agent.server_pid().await;
    let client = agent.client.clone();
    let (public_ip, local) = tokio::join!(
        lookup_public_ip(&client),
        tokio::task::spawn_blocking(move || {
            let exes = discover_server_exes(&install, &bootstrap);
            let local = collect_local(port, tracked);
            (exes, local)
        })
    );
    let (exes, local) = match local {
        Ok(pair) => pair,
        Err(_) => (
            Vec::new(),
            LocalRaw {
                firewall_error: Some("diagnostic task stopped".into()),
                rules: Vec::new(),
                profiles: Vec::new(),
                host: empty_host(Some("diagnostic task stopped".into())),
                tracked_exe: None,
                tracked_children: Vec::new(),
            },
        ),
    };
    build_report(port, expect, public_ip, exes, tracked, local)
}

fn empty_host(err: Option<String>) -> HostSnapshot {
    HostSnapshot {
        listening_error: err.clone(),
        endpoints: Vec::new(),
        lan_error: err,
        addresses: Vec::new(),
        gateways: Vec::new(),
        profiles: Vec::new(),
        processes_error: None,
        processes: Vec::new(),
    }
}

async fn lookup_public_ip(client: &reqwest::Client) -> (Option<String>, String, Vec<String>) {
    let mut errors = Vec::new();
    for (name, url) in IP_SOURCES {
        let request = client.get(*url).send();
        match tokio::time::timeout(LOOKUP_TIMEOUT, request).await {
            Err(_) => errors.push(format!("{name}: timed out")),
            Ok(Err(err)) => errors.push(clip(&format!("{name}: {err}"))),
            Ok(Ok(response)) => {
                let status = response.status();
                if !status.is_success() {
                    errors.push(format!("{name}: HTTP {status}"));
                    continue;
                }
                let body = match tokio::time::timeout(LOOKUP_TIMEOUT, response.text()).await {
                    Ok(Ok(text)) => text.chars().take(512).collect::<String>(),
                    _ => {
                        errors.push(format!("{name}: unreadable body"));
                        continue;
                    }
                };
                if let Some(ip) = parse_public_ipv4(&body) {
                    return (Some(ip), (*name).to_string(), errors);
                }
                errors.push(format!("{name}: no IPv4 in the reply"));
            }
        }
    }
    (None, String::new(), errors)
}

fn collect_local(port: u16, tracked: Option<u32>) -> LocalRaw {
    #[cfg(windows)]
    {
        collect_windows(port, tracked)
    }
    #[cfg(not(windows))]
    {
        let _ = port;
        let (tracked_exe, tracked_children) = unix_tracked(tracked);
        LocalRaw {
            firewall_error: Some("Windows Defender Firewall is read only on Windows".into()),
            rules: Vec::new(),
            profiles: Vec::new(),
            host: HostSnapshot {
                listening_error: Some(
                    "UDP listeners are read with Get-NetUDPEndpoint on Windows".into(),
                ),
                endpoints: Vec::new(),
                lan_error: Some("LAN addresses are read with Get-NetIPAddress on Windows".into()),
                addresses: Vec::new(),
                gateways: Vec::new(),
                profiles: Vec::new(),
                processes_error: None,
                processes: Vec::new(),
            },
            tracked_exe,
            tracked_children,
        }
    }
}

#[cfg(windows)]
fn collect_windows(port: u16, tracked: Option<u32>) -> LocalRaw {
    let port_text = port.to_string();
    let (rules_out, profiles_out, host_out) = std::thread::scope(|scope| {
        let rules = scope.spawn(|| {
            run_command(
                "netsh",
                &[
                    "advfirewall",
                    "firewall",
                    "show",
                    "rule",
                    "name=all",
                    "verbose",
                ],
                None,
                &[],
            )
        });
        let profiles = scope
            .spawn(|| run_command("netsh", &["advfirewall", "show", "allprofiles"], None, &[]));
        let host = scope.spawn(|| {
            run_command(
                "powershell",
                &["-NoProfile", "-NonInteractive", "-Command", "-"],
                Some(HOST_SCRIPT.as_bytes()),
                &[("UXA_DIAG_PORT", port_text.as_str())],
            )
        });
        (rules.join(), profiles.join(), host.join())
    });
    let (firewall_error, rules, profiles) =
        firewall_from_commands(flatten_join(rules_out), flatten_join(profiles_out));
    let host = match flatten_join(host_out) {
        Ok(text) => parse_host_snapshot(&text).unwrap_or_else(|err| HostSnapshot {
            listening_error: Some(err.clone()),
            endpoints: Vec::new(),
            lan_error: Some(err),
            addresses: Vec::new(),
            gateways: Vec::new(),
            profiles: Vec::new(),
            processes_error: Some("process list was not read".into()),
            processes: Vec::new(),
        }),
        Err(err) => HostSnapshot {
            listening_error: Some(err.clone()),
            endpoints: Vec::new(),
            lan_error: Some(err),
            addresses: Vec::new(),
            gateways: Vec::new(),
            profiles: Vec::new(),
            processes_error: Some("process list was not read".into()),
            processes: Vec::new(),
        },
    };
    let tracked_exe = tracked.and_then(|pid| {
        host.processes
            .iter()
            .find(|proc| proc.pid == pid)
            .map(|proc| proc.path.clone())
            .filter(|path| !path.is_empty())
    });
    let tracked_children = tracked
        .map(|pid| children_of(&host.processes, pid))
        .unwrap_or_default();
    LocalRaw {
        firewall_error,
        rules,
        profiles,
        host,
        tracked_exe,
        tracked_children,
    }
}

#[cfg(windows)]
fn flatten_join(joined: std::thread::Result<Result<String, String>>) -> Result<String, String> {
    match joined {
        Ok(inner) => inner,
        Err(_) => Err("diagnostic command stopped".into()),
    }
}

#[cfg(windows)]
fn firewall_from_commands(
    rules_out: Result<String, String>,
    profiles_out: Result<String, String>,
) -> (Option<String>, Vec<RawRule>, Vec<ProfileInfo>) {
    let mut errors = Vec::new();
    let rules = match rules_out {
        Ok(text) => match parse_netsh_rules(&text) {
            Ok(rules) => rules,
            Err(err) => {
                errors.push(err);
                Vec::new()
            }
        },
        Err(err) => {
            errors.push(err);
            Vec::new()
        }
    };
    let profiles = match profiles_out {
        Ok(text) => match parse_netsh_profiles(&text) {
            Ok(profiles) => profiles,
            Err(err) => {
                errors.push(err);
                Vec::new()
            }
        },
        Err(err) => {
            errors.push(err);
            Vec::new()
        }
    };
    let error = if errors.is_empty() {
        None
    } else {
        Some(errors.join("; "))
    };
    (error, rules, profiles)
}

fn build_report(
    port: u16,
    expect: Option<String>,
    public_ip: (Option<String>, String, Vec<String>),
    mut exes: Vec<ServerExe>,
    tracked: Option<u32>,
    local: LocalRaw,
) -> Value {
    if let Some(path) = local.tracked_exe.clone() {
        if !exes.iter().any(|exe| paths_equal(&exe.path, &path)) && !path.is_empty() {
            exes.push(ServerExe {
                role: "tracked",
                file_name: file_name_of(&path),
                exists: path_exists(&path),
                path,
            });
        }
    }
    let (ip, source, ip_errors) = public_ip;
    let matches = match (&expect, &ip) {
        (None, _) | (_, None) => Value::Null,
        (Some(want), Some(got)) => Value::Bool(want == got),
    };
    let active = active_profile_names(&local.host.profiles);
    let mut rules = relevant_rules(&local.rules, port, &exes);
    let truncated = rules.len() > MAX_RULES;
    rules.truncate(MAX_RULES);
    let rule_json: Vec<Value> = rules
        .iter()
        .map(|rule| {
            let specific = is_specific_program(&rule.program);
            let equals = specific && exes.iter().any(|exe| paths_equal(&rule.program, &exe.path));
            let exists = specific && path_exists(&rule.program);
            json!({
                "name": redact_secrets_only(&rule.name),
                "enabled": rule.enabled,
                "direction": rule.direction,
                "action": rule.action,
                "profile": rule.profile,
                "protocol": rule.protocol,
                "localPort": rule.local_port,
                "program": if rule.program.is_empty() {
                    Value::Null
                } else {
                    Value::String(redact_secrets_only(&rule.program))
                },
                "programExists": exists,
                "programEqualsServerExe": equals,
            })
        })
        .collect();
    let profile_json: Vec<Value> = local
        .profiles
        .iter()
        .map(|profile| {
            json!({
                "name": profile.name,
                "enabled": profile.enabled,
                "defaultInboundAction": profile.default_inbound,
                "active": active.iter().any(|name| name.eq_ignore_ascii_case(&profile.name)),
            })
        })
        .collect();
    let exe_json: Vec<Value> = exes
        .iter()
        .map(|exe| {
            json!({
                "role": exe.role,
                "path": redact_secrets_only(&exe.path),
                "exists": exe.exists,
            })
        })
        .collect();
    let child_ids: Vec<u32> = local.tracked_children.iter().map(|proc| proc.pid).collect();
    let children: Vec<Value> = local
        .tracked_children
        .iter()
        .map(|proc| {
            json!({
                "pid": proc.pid,
                "parentPid": proc.ppid,
                "name": redact_secrets_only(&proc.name),
                "path": redact_secrets_only(&proc.path),
            })
        })
        .collect();
    let endpoints: Vec<Value> = local
        .host
        .endpoints
        .iter()
        .map(|ep| {
            let is_tracked = tracked == Some(ep.pid);
            json!({
                "localAddress": ep.local_address,
                "localPort": ep.local_port,
                "pid": ep.pid,
                "processName": redact_secrets_only(&ep.process_name),
                "processPath": redact_secrets_only(&ep.process_path),
                "isTrackedProcess": is_tracked,
                "isTrackedOrChild": is_tracked || child_ids.contains(&ep.pid),
            })
        })
        .collect();
    let addresses: Vec<Value> = local
        .host
        .addresses
        .iter()
        .filter(|addr| keep_lan_address(&addr.address))
        .map(|addr| {
            json!({
                "adapter": redact_secrets_only(&addr.adapter),
                "address": addr.address,
                "prefixLength": addr.prefix,
            })
        })
        .collect();
    let gateway = pick_gateway(&local.host.gateways);
    let category = network_category(
        &local.host.profiles,
        gateway.as_ref().map(|gw| gw.adapter.as_str()),
    );
    let conn_profiles: Vec<Value> = local
        .host
        .profiles
        .iter()
        .map(|profile| {
            json!({
                "name": redact_secrets_only(&profile.name),
                "adapter": redact_secrets_only(&profile.adapter),
                "category": friendly_category(&profile.category),
            })
        })
        .collect();
    let firewall_source =
        if !local.rules.is_empty() || (local.firewall_error.is_none() && cfg!(windows)) {
            Value::String("netsh".into())
        } else {
            Value::Null
        };
    let firewall_error = local.firewall_error.map(|err| redact_secrets_only(&err));
    let listening_error = match (local.host.listening_error, local.host.processes_error) {
        (Some(left), Some(right)) => Some(format!("{left}; {right}")),
        (left, right) => left.or(right),
    }
    .map(|err| redact_secrets_only(&err));
    let lan_error = local.host.lan_error.map(|err| redact_secrets_only(&err));
    let tracked_exe = local.tracked_exe.map(|path| redact_secrets_only(&path));
    let default_gateway = gateway.map(|gw| gw.hop);
    json!({
        "port": port,
        "expect": expect,
        "publicIp": ip,
        "publicIpSource": if source.is_empty() { Value::Null } else { Value::String(source) },
        "publicIpMatchesExpected": matches,
        "publicIpErrors": ip_errors.into_iter().map(|err| redact_secrets_only(&err)).collect::<Vec<_>>(),
        "firewall": {
            "error": firewall_error,
            "source": firewall_source,
            "activeProfiles": active,
            "profiles": profile_json,
            "rules": rule_json,
            "rulesTruncated": truncated,
            "serverExes": exe_json,
        },
        "listening": {
            "error": listening_error,
            "trackedPid": tracked,
            "trackedExe": tracked_exe,
            "children": children,
            "endpoints": endpoints,
        },
        "lan": {
            "error": lan_error,
            "addresses": addresses,
            "defaultGateway": default_gateway,
            "networkCategory": category,
            "profiles": conn_profiles,
        },
    })
}

fn relevant_rules(rules: &[RawRule], port: u16, exes: &[ServerExe]) -> Vec<RawRule> {
    let mut kept: Vec<RawRule> = rules
        .iter()
        .filter(|rule| rule_covers(rule, port, exes))
        .cloned()
        .collect();
    kept.sort_by(|a, b| {
        block_rank(&a.action)
            .cmp(&block_rank(&b.action))
            .then(a.name.cmp(&b.name))
    });
    kept
}

fn block_rank(action: &str) -> u8 {
    if action.eq_ignore_ascii_case("block") {
        0
    } else {
        1
    }
}

fn rule_covers(rule: &RawRule, port: u16, exes: &[ServerExe]) -> bool {
    if !is_inbound(&rule.direction) {
        return false;
    }
    let by_port = protocol_covers_udp(&rule.protocol) && port_covers(&rule.local_port, port);
    by_port || program_covers(&rule.program, exes)
}

fn is_inbound(direction: &str) -> bool {
    let direction = direction.trim();
    direction.eq_ignore_ascii_case("in") || direction.eq_ignore_ascii_case("inbound")
}

fn protocol_covers_udp(protocol: &str) -> bool {
    let protocol = protocol.trim();
    protocol.eq_ignore_ascii_case("udp") || protocol.eq_ignore_ascii_case("any") || protocol == "17"
}

fn port_covers(spec: &str, port: u16) -> bool {
    let spec = spec.trim();
    if spec.is_empty() || spec.eq_ignore_ascii_case("any") || spec == "*" {
        return true;
    }
    for part in spec.split([',', ';']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (start.trim().parse::<u16>(), end.trim().parse::<u16>()) {
                let (lo, hi) = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                if (lo..=hi).contains(&port) {
                    return true;
                }
            }
            continue;
        }
        if part.parse::<u16>().ok() == Some(port) {
            return true;
        }
    }
    false
}

fn program_covers(program: &str, exes: &[ServerExe]) -> bool {
    if !is_specific_program(program) {
        return false;
    }
    let file = file_name_of(program);
    exes.iter().any(|exe| {
        paths_equal(program, &exe.path)
            || (!file.is_empty()
                && !exe.file_name.is_empty()
                && file.eq_ignore_ascii_case(&exe.file_name))
    })
}

fn is_specific_program(program: &str) -> bool {
    let program = program.trim().trim_matches('"');
    !program.is_empty()
        && !program.eq_ignore_ascii_case("any")
        && !program.eq_ignore_ascii_case("system")
}

fn path_exists(path: &str) -> bool {
    if !is_specific_program(path) {
        return false;
    }
    let path = path.trim().trim_matches('"');
    Path::new(path).is_file()
}

fn paths_equal(left: &str, right: &str) -> bool {
    let left = norm_path(left);
    let right = norm_path(right);
    !left.is_empty() && left == right
}

fn norm_path(path: &str) -> String {
    let path = path.trim().trim_matches('"');
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn file_name_of(path: &str) -> String {
    let path = path.trim().trim_matches('"').replace('/', "\\");
    path.rsplit('\\').next().unwrap_or("").trim().to_string()
}

fn keep_lan_address(address: &str) -> bool {
    let Some(address) = parse_exact_ipv4(address) else {
        return false;
    };
    if address == "0.0.0.0" || address.starts_with("127.") || address.starts_with("169.254.") {
        return false;
    }
    true
}

fn pick_gateway(gateways: &[Gateway]) -> Option<Gateway> {
    let mut best: Option<&Gateway> = None;
    for gateway in gateways {
        if !keep_lan_address(&gateway.hop) && parse_exact_ipv4(&gateway.hop).is_none() {
            continue;
        }
        if gateway.hop == "0.0.0.0" || parse_exact_ipv4(&gateway.hop).is_none() {
            continue;
        }
        best = Some(match best {
            None => gateway,
            Some(current) if gateway.metric < current.metric => gateway,
            Some(current) => current,
        });
    }
    best.cloned()
}

fn network_category(profiles: &[ConnProfile], gateway_adapter: Option<&str>) -> Value {
    let chosen = gateway_adapter
        .and_then(|adapter| {
            profiles
                .iter()
                .find(|profile| profile.adapter.eq_ignore_ascii_case(adapter))
        })
        .or(profiles.first());
    match chosen {
        Some(profile) if !profile.category.trim().is_empty() => {
            Value::String(friendly_category(&profile.category))
        }
        _ => Value::Null,
    }
}

fn friendly_category(category: &str) -> String {
    if category.eq_ignore_ascii_case("domainauthenticated")
        || category.eq_ignore_ascii_case("domain")
    {
        "Domain".into()
    } else if category.eq_ignore_ascii_case("private") {
        "Private".into()
    } else if category.eq_ignore_ascii_case("public") {
        "Public".into()
    } else {
        category.trim().to_string()
    }
}

fn active_profile_names(profiles: &[ConnProfile]) -> Vec<String> {
    let mut names = Vec::new();
    for profile in profiles {
        let name = friendly_category(&profile.category);
        if name.is_empty() {
            continue;
        }
        if !names
            .iter()
            .any(|have: &String| have.eq_ignore_ascii_case(&name))
        {
            names.push(name);
        }
    }
    names
}

fn children_of(procs: &[ProcInfo], root: u32) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let mut frontier = vec![(root, 0u8)];
    let mut seen = vec![root];
    while let Some((parent, depth)) = frontier.pop() {
        if depth >= 4 || out.len() >= MAX_CHILDREN {
            continue;
        }
        for proc in procs {
            if proc.ppid == parent && proc.pid != 0 && !seen.contains(&proc.pid) {
                seen.push(proc.pid);
                out.push(proc.clone());
                frontier.push((proc.pid, depth + 1));
                if out.len() >= MAX_CHILDREN {
                    break;
                }
            }
        }
    }
    out
}

fn parse_netsh_rules(text: &str) -> Result<Vec<RawRule>, String> {
    if command_refused(text) {
        return Err(clip(text));
    }
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut current: Option<Vec<String>> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim_start().starts_with("Rule Name:") {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            current = Some(vec![line.to_string()]);
        } else if let Some(block) = current.as_mut() {
            block.push(line.to_string());
        }
    }
    if let Some(block) = current {
        blocks.push(block);
    }
    if blocks.is_empty() {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        return Err("netsh output did not include rule names".into());
    }
    let rules = blocks
        .iter()
        .map(|lines| {
            let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
            RawRule {
                name: field(&refs, "Rule Name"),
                enabled: yes(&field(&refs, "Enabled")),
                direction: field(&refs, "Direction"),
                action: field(&refs, "Action"),
                profile: field(&refs, "Profiles"),
                protocol: field(&refs, "Protocol"),
                local_port: field(&refs, "LocalPort"),
                program: {
                    let program = field(&refs, "Program");
                    if program.is_empty() {
                        field(&refs, "Application")
                    } else {
                        program
                    }
                },
            }
        })
        .filter(|rule| !rule.name.is_empty())
        .collect();
    Ok(rules)
}

fn parse_netsh_profiles(text: &str) -> Result<Vec<ProfileInfo>, String> {
    if command_refused(text) {
        return Err(clip(text));
    }
    let mut profiles = Vec::new();
    let mut name = String::new();
    let mut enabled = false;
    let mut policy = String::new();
    let mut in_block = false;
    let flush = |name: &mut String,
                 enabled: &mut bool,
                 policy: &mut String,
                 in_block: &mut bool,
                 profiles: &mut Vec<ProfileInfo>| {
        if *in_block && !name.is_empty() {
            profiles.push(ProfileInfo {
                name: name.clone(),
                enabled: *enabled,
                default_inbound: inbound_action(policy),
            });
        }
        name.clear();
        *enabled = false;
        policy.clear();
        *in_block = false;
    };
    for line in text.lines() {
        let line = line.trim_end_matches('\r').trim();
        if let Some(header) = line.strip_suffix("Profile Settings:") {
            flush(
                &mut name,
                &mut enabled,
                &mut policy,
                &mut in_block,
                &mut profiles,
            );
            name = header.trim().to_string();
            in_block = true;
            continue;
        }
        if !in_block {
            continue;
        }
        if let Some(rest) = line.strip_prefix("State") {
            enabled = yes(rest.trim().trim_start_matches(':'));
        } else if let Some(rest) = line.strip_prefix("Firewall Policy") {
            policy = rest.trim().trim_start_matches(':').trim().to_string();
        }
    }
    flush(
        &mut name,
        &mut enabled,
        &mut policy,
        &mut in_block,
        &mut profiles,
    );
    if profiles.is_empty() && !text.trim().is_empty() {
        return Err("netsh output did not include firewall profiles".into());
    }
    Ok(profiles)
}

fn inbound_action(policy: &str) -> String {
    let first = policy.split(',').next().unwrap_or("").trim();
    if first.to_ascii_lowercase().starts_with("allow") {
        "Allow".into()
    } else if first.to_ascii_lowercase().starts_with("block") {
        "Block".into()
    } else if first.is_empty() {
        String::new()
    } else {
        first.to_string()
    }
}

fn yes(value: &str) -> bool {
    let value = value.trim();
    value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("on")
        || value.eq_ignore_ascii_case("enabled")
}

fn field(lines: &[&str], name: &str) -> String {
    let prefix = format!("{name}:");
    for line in lines {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            return rest.trim().to_string();
        }
    }
    String::new()
}

fn command_refused(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("requires elevation") || lower.contains("access is denied")
}

fn parse_host_snapshot(text: &str) -> Result<HostSnapshot, String> {
    let json_text = extract_json(text)?;
    let value: Value = serde_json::from_str(json_text).map_err(|err| err.to_string())?;
    let obj = value
        .as_object()
        .ok_or_else(|| "host snapshot was not an object".to_string())?;
    Ok(HostSnapshot {
        listening_error: string_field(obj.get("listeningError")),
        endpoints: array_field(obj.get("endpoints"))
            .into_iter()
            .filter_map(parse_endpoint)
            .collect(),
        lan_error: string_field(obj.get("lanError")),
        addresses: array_field(obj.get("addresses"))
            .into_iter()
            .filter_map(parse_address)
            .collect(),
        gateways: array_field(obj.get("gateways"))
            .into_iter()
            .filter_map(parse_gateway)
            .collect(),
        profiles: array_field(obj.get("profiles"))
            .into_iter()
            .filter_map(parse_conn_profile)
            .collect(),
        processes_error: string_field(obj.get("processesError")),
        processes: array_field(obj.get("processes"))
            .into_iter()
            .filter_map(parse_proc)
            .collect(),
    })
}

fn extract_json(text: &str) -> Result<&str, String> {
    let start = text
        .find(['{', '['])
        .ok_or_else(|| "no JSON in command output".to_string())?;
    let end = text
        .rfind(['}', ']'])
        .ok_or_else(|| "command output JSON was cut off".to_string())?;
    if end < start {
        return Err("command output JSON was cut off".into());
    }
    Ok(&text[start..=end])
}

fn array_field(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Array(items)) => items.iter().collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(other) => vec![other],
    }
}

fn string_field(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.trim().is_empty() => Some(text.trim().to_string()),
        _ => None,
    }
}

fn parse_endpoint(value: &Value) -> Option<Endpoint> {
    Some(Endpoint {
        local_address: value
            .get("localAddress")?
            .as_str()
            .unwrap_or("")
            .to_string(),
        local_port: value.get("localPort")?.as_u64()? as u16,
        pid: value.get("owningProcess")?.as_u64()? as u32,
        process_name: value
            .get("processName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        process_path: value
            .get("processPath")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_address(value: &Value) -> Option<LanAddr> {
    Some(LanAddr {
        adapter: value
            .get("interfaceAlias")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        address: value.get("ipAddress")?.as_str().unwrap_or("").to_string(),
        prefix: value
            .get("prefixLength")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
    })
}

fn parse_gateway(value: &Value) -> Option<Gateway> {
    Some(Gateway {
        adapter: value
            .get("interfaceAlias")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        hop: value.get("nextHop")?.as_str().unwrap_or("").to_string(),
        metric: value
            .get("routeMetric")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX) as u32,
    })
}

fn parse_conn_profile(value: &Value) -> Option<ConnProfile> {
    Some(ConnProfile {
        name: value
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        adapter: value
            .get("interfaceAlias")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        category: value
            .get("networkCategory")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_proc(value: &Value) -> Option<ProcInfo> {
    Some(ProcInfo {
        pid: value.get("processId")?.as_u64()? as u32,
        ppid: value
            .get("parentProcessId")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        name: value
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        path: value
            .get("executablePath")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn discover_server_exes(install: &Path, bootstrap_name: &str) -> Vec<ServerExe> {
    let mut found = Vec::new();
    let mut seen = 0usize;
    walk_exes(install, bootstrap_name, 6, &mut found, &mut seen);
    found.truncate(16);
    found
}

fn walk_exes(
    dir: &Path,
    bootstrap_name: &str,
    depth: usize,
    found: &mut Vec<ServerExe>,
    seen: &mut usize,
) {
    if depth == 0 || *seen > MAX_WALK || found.len() >= 16 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        *seen += 1;
        if *seen > MAX_WALK || found.len() >= 16 {
            return;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            if name.starts_with('.')
                || name.eq_ignore_ascii_case("Saved")
                || name.eq_ignore_ascii_case("Intermediate")
                || name.eq_ignore_ascii_case("Engine")
            {
                continue;
            }
            walk_exes(&path, bootstrap_name, depth - 1, found, seen);
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if !lower.ends_with(".exe") {
            continue;
        }
        let is_bootstrap = name.eq_ignore_ascii_case(bootstrap_name);
        let is_shipping = lower.contains("win64-shipping");
        let parent_win64 = path
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|parent| parent.eq_ignore_ascii_case("Win64"))
            .unwrap_or(false);
        let is_win64_server = parent_win64 && lower.contains("server");
        if !(is_bootstrap || is_shipping || is_win64_server) {
            continue;
        }
        let display = path.display().to_string();
        if found.iter().any(|exe| paths_equal(&exe.path, &display)) {
            continue;
        }
        let role = if is_bootstrap {
            "bootstrap"
        } else {
            "shipping"
        };
        found.push(ServerExe {
            role,
            file_name: name.to_string(),
            exists: true,
            path: display,
        });
    }
}

#[cfg(not(windows))]
fn unix_tracked(tracked: Option<u32>) -> (Option<String>, Vec<ProcInfo>) {
    let Some(pid) = tracked else {
        return (None, Vec::new());
    };
    let exe = fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|path| path.display().to_string());
    let procs = unix_processes();
    (exe, children_of(&procs, pid))
}

#[cfg(not(windows))]
fn unix_processes() -> Vec<ProcInfo> {
    let mut procs = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return procs;
    };
    for entry in entries.flatten().take(2000) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let ppid = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .and_then(|rest| rest.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let path = fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        procs.push(ProcInfo {
            pid,
            ppid,
            name: comm,
            path,
        });
    }
    procs
}

#[cfg(windows)]
struct CmdText {
    text: String,
}

#[cfg(windows)]
fn run_command(
    program: &str,
    args: &[&str],
    stdin_bytes: Option<&[u8]>,
    env: &[(&str, &str)],
) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stdin_bytes.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }
    for (key, value) in env {
        command.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|err| format!("could not run {program}: {err}"))?;
    if let Some(bytes) = stdin_bytes {
        if let Some(mut input) = child.stdin.take() {
            use std::io::Write;
            let _ = input.write_all(bytes);
        }
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{program} has no output"))?;
    let stderr = child.stderr.take();
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = out_tx.send(read_capped(stdout));
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let text = match stderr {
            Some(reader) => read_capped(reader),
            None => CmdText {
                text: String::new(),
            },
        };
        let _ = err_tx.send(text);
    });
    let started = Instant::now();
    let stdout_text = loop {
        match out_rx.try_recv() {
            Ok(CmdText { text }) => break text,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if started.elapsed() >= CMD_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{program} timed out after {}s",
                        CMD_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                let _ = child.wait();
                return Err(format!("{program} produced no output"));
            }
        }
    };
    let stderr_text = match err_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(text) => text.text,
        Err(_) => String::new(),
    };
    let status = child.wait().map_err(|err| format!("{program}: {err}"))?;
    if !status.success() && stdout_text.trim().is_empty() {
        let detail = if stderr_text.trim().is_empty() {
            status.to_string()
        } else {
            clip(&stderr_text)
        };
        return Err(format!("{program} failed: {detail}"));
    }
    if stdout_text.trim().is_empty() && !stderr_text.trim().is_empty() {
        return Err(clip(&stderr_text));
    }
    Ok(stdout_text)
}

#[cfg(windows)]
fn read_capped<R: Read>(mut reader: R) -> CmdText {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < MAX_CMD_BYTES {
                    let room = MAX_CMD_BYTES - buf.len();
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
                if buf.len() >= MAX_CMD_BYTES {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    let mut text = decode_output(&buf);
    if truncated {
        text.push_str("\n[output truncated]");
    }
    CmdText { text }
}

#[cfg(windows)]
fn decode_output(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16_le(&bytes[2..]);
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&bytes[3..]).into_owned();
    }
    let pairs = bytes.len() / 2;
    if pairs > 8 {
        let nul_odd = bytes
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|byte| **byte == 0)
            .count();
        if nul_odd * 2 > pairs {
            return decode_utf16_le(bytes);
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(windows)]
fn decode_utf16_le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
}

fn clip(text: &str) -> String {
    let trimmed = text.trim().replace('\r', "");
    let mut out: String = trimmed.chars().take(300).collect();
    if trimmed.chars().count() > 300 {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = "\
Rule Name:                            Core Networking DNS
----------------------------------------------------------------------
Enabled:                              Yes
Direction:                            Out
Profiles:                             Domain,Private,Public
Grouping:
LocalIP:                              Any
RemoteIP:                             Any
Protocol:                             UDP
LocalPort:                            Any
RemotePort:                           53
Edge traversal:                       No
Action:                               Allow

Rule Name:                            Friday Night Fights UDP 7777
----------------------------------------------------------------------
Enabled:                              Yes
Direction:                            In
Profiles:                             Private
Grouping:
LocalIP:                              Any
RemoteIP:                             Any
Protocol:                             UDP
LocalPort:                            7777
RemotePort:                           Any
Edge traversal:                       No
Action:                               Allow

Rule Name:                            Block token=sekrit server
----------------------------------------------------------------------
Enabled:                              Yes
Direction:                            In
Profiles:                             Public
Grouping:
LocalIP:                              Any
RemoteIP:                             Any
Protocol:                             Any
LocalPort:                            Any
RemotePort:                           Any
Edge traversal:                       No
Program:                              C:\\FNF\\Server\\FridayNightFights\\Binaries\\Win64\\FridayNightFightsServer-Win64-Shipping.exe
Action:                               Block

Rule Name:                            Allow UDP range
----------------------------------------------------------------------
Enabled:                              No
Direction:                            In
Profiles:                             Domain,Private,Public
Grouping:
LocalIP:                              Any
RemoteIP:                             Any
Protocol:                             UDP
LocalPort:                            7770-7780
RemotePort:                           Any
Edge traversal:                       No
Action:                               Allow

Rule Name:                            Web
----------------------------------------------------------------------
Enabled:                              Yes
Direction:                            In
Profiles:                             Public
Grouping:
LocalIP:                              Any
RemoteIP:                             Any
Protocol:                             TCP
LocalPort:                            80
RemotePort:                           Any
Edge traversal:                       No
Action:                               Allow
";

    const PROFILES: &str = "\
Domain Profile Settings:
----------------------------------------------------------------------
State                                 ON
Firewall Policy                       BlockInbound,AllowOutbound

Private Profile Settings:
----------------------------------------------------------------------
State                                 ON
Firewall Policy                       BlockInbound,AllowOutbound

Public Profile Settings:
----------------------------------------------------------------------
State                                 OFF
Firewall Policy                       AllowInbound,AllowOutbound
";

    fn sample_exes(shipping: &str) -> Vec<ServerExe> {
        vec![
            ServerExe {
                role: "bootstrap",
                path: r"C:\FNF\Server\FridayNightFightsServer.exe".into(),
                file_name: "FridayNightFightsServer.exe".into(),
                exists: false,
            },
            ServerExe {
                role: "shipping",
                path: shipping.into(),
                file_name: "FridayNightFightsServer-Win64-Shipping.exe".into(),
                exists: Path::new(shipping).is_file(),
            },
        ]
    }

    #[test]
    fn port_allow_list_and_ipv4_parsing() {
        assert_eq!(parse_diag_port("", 7777).unwrap(), 7777);
        assert_eq!(parse_diag_port("7777", 27015).unwrap(), 7777);
        assert_eq!(parse_diag_port("27015", 27015).unwrap(), 27015);
        assert!(parse_diag_port("80", 7777).is_err());
        assert!(parse_diag_port("nope", 7777).is_err());
        assert_eq!(
            parse_exact_ipv4(" 203.0.113.10 ").as_deref(),
            Some("203.0.113.10")
        );
        assert!(parse_exact_ipv4("203.0.113.10 extra").is_none());
        assert!(parse_exact_ipv4("01.2.3.4").is_none());
        assert_eq!(
            parse_public_ipv4("\n  198.51.100.7\n").as_deref(),
            Some("198.51.100.7")
        );
        assert!(parse_public_ipv4("<html>no address</html>").is_none());
        assert!(!port_covers("80", 7777));
        assert!(port_covers("7770-7780", 7777));
        assert!(port_covers("80,7777", 7777));
        assert!(!port_covers("RPC", 7777));
        assert!(port_covers("Any", 7777));
    }

    #[test]
    fn netsh_and_host_snapshot_keep_addresses_and_redact_secrets() {
        let temp = tempfile::tempdir().unwrap();
        let shipping = temp
            .path()
            .join("FridayNightFightsServer-Win64-Shipping.exe");
        fs::write(&shipping, b"MZ").unwrap();
        let shipping_text = shipping.display().to_string();
        let rules_text = RULES.replace(
            r"C:\FNF\Server\FridayNightFights\Binaries\Win64\FridayNightFightsServer-Win64-Shipping.exe",
            &shipping_text,
        );
        let rules = parse_netsh_rules(&rules_text).unwrap();
        assert_eq!(rules.len(), 5, "{rules:?}");
        let profiles = parse_netsh_profiles(PROFILES).unwrap();
        assert_eq!(profiles.len(), 3);
        assert!(profiles.iter().any(|profile| {
            profile.name == "Private" && profile.enabled && profile.default_inbound == "Block"
        }));
        assert!(profiles.iter().any(|profile| {
            profile.name == "Public" && !profile.enabled && profile.default_inbound == "Allow"
        }));
        let exes = sample_exes(&shipping_text);
        let kept = relevant_rules(&rules, 7777, &exes);
        let names: Vec<&str> = kept.iter().map(|rule| rule.name.as_str()).collect();
        assert!(names.contains(&"Friday Night Fights UDP 7777"), "{names:?}");
        assert!(
            names.iter().any(|name| name.contains("token=sekrit")),
            "{names:?}"
        );
        assert!(names.contains(&"Allow UDP range"), "{names:?}");
        assert!(!names.contains(&"Core Networking DNS"), "{names:?}");
        assert!(!names.contains(&"Web"), "{names:?}");
        assert_eq!(kept[0].action.to_ascii_lowercase(), "block");

        let host = format!(
            r#"{{"listeningError":null,"endpoints":{{"localAddress":"0.0.0.0","localPort":7777,"owningProcess":4242,"processName":"FridayNightFightsServer-Win64-Shipping.exe","processPath":{path}}},"lanError":null,"addresses":[{{"interfaceAlias":"Ethernet","ipAddress":"192.168.1.50","prefixLength":24}},{{"interfaceAlias":"Loopback","ipAddress":"127.0.0.1","prefixLength":8}},{{"interfaceAlias":"Link","ipAddress":"169.254.9.9","prefixLength":16}}],"gateways":[{{"interfaceAlias":"Ethernet","nextHop":"192.168.1.1","routeMetric":25}},{{"interfaceAlias":"Ethernet","nextHop":"0.0.0.0","routeMetric":0}}],"profiles":[{{"name":"Home","interfaceAlias":"Ethernet","networkCategory":"Private"}}],"processesError":null,"processes":[{{"processId":100,"parentProcessId":1,"name":"FridayNightFightsServer.exe","executablePath":"C:\\FNF\\Server\\FridayNightFightsServer.exe"}},{{"processId":4242,"parentProcessId":100,"name":"FridayNightFightsServer-Win64-Shipping.exe","executablePath":{path}}},{{"processId":50,"parentProcessId":1,"name":"other.exe","executablePath":"C:\\Windows\\other.exe"}}]}}"#,
            path = serde_json::to_string(&shipping_text).unwrap(),
        );
        let host = parse_host_snapshot(&host).unwrap();
        assert_eq!(host.endpoints.len(), 1);
        assert_eq!(host.endpoints[0].pid, 4242);
        assert_eq!(host.processes.len(), 3);
        let children = children_of(&host.processes, 100);
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].pid, 4242);

        let report = build_report(
            7777,
            Some("203.0.113.10".into()),
            (
                Some("198.51.100.20".into()),
                "api.ipify.org".into(),
                vec!["ifconfig.me: not used".into()],
            ),
            exes,
            Some(100),
            LocalRaw {
                firewall_error: None,
                rules,
                profiles,
                host,
                tracked_exe: Some(r"C:\FNF\Server\FridayNightFightsServer.exe".into()),
                tracked_children: children,
            },
        );
        let body = report.to_string();
        assert!(body.contains("198.51.100.20"), "{body}");
        assert!(body.contains("192.168.1.50"), "{body}");
        assert!(body.contains("192.168.1.1"), "{body}");
        assert!(!body.contains("127.0.0.1"), "{body}");
        assert!(!body.contains("169.254.9.9"), "{body}");
        assert!(!body.contains("sekrit"), "{body}");
        assert!(body.contains("[redacted]"), "{body}");
        assert_eq!(report["publicIpMatchesExpected"], false);
        assert_eq!(report["publicIpSource"], "api.ipify.org");
        assert_eq!(report["lan"]["networkCategory"], "Private");
        assert_eq!(report["lan"]["defaultGateway"], "192.168.1.1");
        assert_eq!(report["firewall"]["activeProfiles"][0], "Private");
        let block = report["firewall"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule["action"] == "Block")
            .unwrap();
        assert_eq!(block["programExists"], true);
        assert_eq!(block["programEqualsServerExe"], true);
        assert!(block["program"]
            .as_str()
            .unwrap()
            .contains("Win64-Shipping"));
        let endpoint = &report["listening"]["endpoints"][0];
        assert_eq!(endpoint["localAddress"], "0.0.0.0");
        assert_eq!(endpoint["isTrackedProcess"], false);
        assert_eq!(endpoint["isTrackedOrChild"], true);
        assert_eq!(report["listening"]["trackedPid"], 100);
        assert_eq!(report["listening"]["children"][0]["pid"], 4242);

        let unknown = build_report(
            7777,
            None,
            (None, String::new(), vec!["api.ipify.org: timed out".into()]),
            Vec::new(),
            None,
            LocalRaw {
                firewall_error: Some("token=hide-me failed".into()),
                rules: Vec::new(),
                profiles: Vec::new(),
                host: empty_host(None),
                tracked_exe: None,
                tracked_children: Vec::new(),
            },
        );
        assert!(unknown["publicIpMatchesExpected"].is_null());
        assert!(unknown["publicIp"].is_null());
        let firewall_error = unknown["firewall"]["error"].as_str().unwrap();
        assert!(!firewall_error.contains("hide-me"), "{firewall_error}");
        assert!(firewall_error.contains("[redacted]"), "{firewall_error}");
    }

    #[test]
    fn discovers_bootstrap_and_shipping_and_skips_saved() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("install");
        fs::create_dir_all(install.join("FridayNightFights/Binaries/Win64")).unwrap();
        fs::create_dir_all(install.join("Saved")).unwrap();
        fs::write(install.join("FridayNightFightsServer.exe"), b"boot").unwrap();
        fs::write(
            install.join(
                "FridayNightFights/Binaries/Win64/FridayNightFightsServer-Win64-Shipping.exe",
            ),
            b"ship",
        )
        .unwrap();
        fs::write(install.join("Saved/skip-Win64-Shipping.exe"), b"no").unwrap();
        fs::create_dir_all(install.join("tools")).unwrap();
        fs::write(install.join("tools/Unrelated.exe"), b"no").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                install.join("FridayNightFightsServer.exe"),
                install.join("linked-Win64-Shipping.exe"),
            )
            .unwrap();
        }
        let found = discover_server_exes(&install, "FridayNightFightsServer.exe");
        assert!(found.iter().any(|exe| exe.role == "bootstrap"));
        assert!(found
            .iter()
            .any(|exe| { exe.role == "shipping" && exe.path.contains("Win64-Shipping") }));
        assert!(found.iter().all(|exe| !exe.path.contains("skip-")));
        assert!(found.iter().all(|exe| !exe.path.contains("linked-")));
        assert!(found.iter().all(|exe| !exe.path.contains("Unrelated")));
    }
}
