//! Signed, read-only diagnostics. Callers never supply a filesystem path.
//! Every file this module opens is discovered under the install tree's
//! Saved/Logs or Saved/Crashes, or under the agent work directory, and is
//! checked again with a canonical prefix before it is read.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

pub const MAX_DUMP_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_CHUNK_BYTES: u64 = 8 * 1024 * 1024;
const MAX_COPY_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    Server,
    Agent,
    Ue,
    Failed,
}

pub fn parse_log_source(value: &str) -> Result<LogSource, String> {
    match value.trim() {
        "" | "server" => Ok(LogSource::Server),
        "agent" => Ok(LogSource::Agent),
        "ue" => Ok(LogSource::Ue),
        "failed" => Ok(LogSource::Failed),
        _ => Err("source must be server, agent, ue, or failed".into()),
    }
}

/// A single path segment. Rejects empty, `.`, `..`, slashes, and colons.
pub fn plain_name(value: &str) -> Result<&str, String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 80
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err("name is not a single file or directory name".into());
    }
    Ok(value)
}

pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = secret_at(text, i) {
            out.push_str("[redacted]");
            i += len;
            continue;
        }
        if let Some(len) = ipv4_at(text, i) {
            let slice = &text[i..i + len];
            if slice == "0.0.0.0" || slice.starts_with("127.") {
                out.push_str(slice);
            } else {
                out.push_str("[redacted]");
            }
            i += len;
            continue;
        }
        if let Some(len) = ipv6_at(text, i) {
            out.push_str("[redacted]");
            i += len;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `key=` / `token=` (and the other secret labels) only. Addresses stay so a
/// diagnostic reply can show the operator the IPs they asked for.
pub fn redact_secrets_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if let Some(len) = secret_at(text, i) {
            out.push_str("[redacted]");
            i += len;
            continue;
        }
        let ch = text[i..].chars().next().unwrap_or('\u{FFFD}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn secret_at(text: &str, start: usize) -> Option<usize> {
    const KEYS: &[&str] = &[
        "key",
        "token",
        "password",
        "secret",
        "authorization",
        "bearer",
    ];
    let rest = &text[start..];
    let lower = rest.to_ascii_lowercase();
    for key in KEYS {
        if !lower.starts_with(key) {
            continue;
        }
        let after = rest.get(key.len()..)?;
        let after_trim = after.trim_start_matches([' ', '\t']);
        let sep = after_trim.chars().next()?;
        if sep != '=' && sep != ':' {
            continue;
        }
        let sep_off = key.len() + (after.len() - after_trim.len()) + sep.len_utf8();
        let value = rest.get(sep_off..).unwrap_or("");
        let skipped = value.len() - value.trim_start_matches([' ', '\t']).len();
        let value = value.get(skipped..).unwrap_or("");
        let value_len = value
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace() || *ch == '"' || *ch == '\'')
            .map(|(idx, _)| idx)
            .unwrap_or(value.len());
        if value_len == 0 {
            continue;
        }
        return Some(sep_off + skipped + value_len);
    }
    None
}

pub(crate) fn ipv4_at(text: &str, start: usize) -> Option<usize> {
    let rest = text.get(start..)?;
    let mut parts = 0;
    let mut idx = 0;
    let bytes = rest.as_bytes();
    while parts < 4 {
        if parts > 0 {
            if bytes.get(idx) != Some(&b'.') {
                return None;
            }
            idx += 1;
        }
        let start_digits = idx;
        let mut value: u32 = 0;
        let mut digits = 0;
        while let Some(b) = bytes.get(idx) {
            if !b.is_ascii_digit() {
                break;
            }
            if digits == 3 {
                return None;
            }
            value = value * 10 + u32::from(b - b'0');
            if value > 255 {
                return None;
            }
            digits += 1;
            idx += 1;
        }
        if digits == 0 || (digits > 1 && bytes[start_digits] == b'0') {
            return None;
        }
        parts += 1;
    }
    if parts != 4 {
        return None;
    }
    if bytes
        .get(idx)
        .is_some_and(|b| b.is_ascii_digit() || *b == b'.')
    {
        return None;
    }
    Some(idx)
}

fn ipv6_at(text: &str, start: usize) -> Option<usize> {
    let rest = text.get(start..)?;
    let end = rest
        .char_indices()
        .find(|(_, ch)| !(ch.is_ascii_hexdigit() || *ch == ':'))
        .map(|(idx, _)| idx)
        .unwrap_or(rest.len());
    if end < 2 {
        return None;
    }
    let token = &rest[..end];
    let colons = token.chars().filter(|ch| *ch == ':').count();
    if token.contains("::") && colons >= 2 {
        return Some(end);
    }
    if colons == 7 && token.chars().all(|ch| ch.is_ascii_hexdigit() || ch == ':') {
        return Some(end);
    }
    None
}

pub fn exit_text(code: Option<i32>) -> String {
    match code {
        None => "no exit code".into(),
        Some(code) => {
            let bits = code as u32;
            let name = match bits {
                0xC0000135 => "STATUS_DLL_NOT_FOUND",
                0xC000007B => "STATUS_INVALID_IMAGE_FORMAT",
                0xC0000139 => "STATUS_ENTRYPOINT_NOT_FOUND",
                _ => "",
            };
            if name.is_empty() {
                format!("exit {code}")
            } else {
                format!("exit {code} (0x{bits:08X} {name})")
            }
        }
    }
}

fn canonical_inside(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    if path.starts_with(&root) {
        Some(path)
    } else {
        None
    }
}

fn safe_tag(tag: &str) -> String {
    let mut out: String = tag
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '.' || *ch == '-' || *ch == '_')
        .take(40)
        .collect();
    if out.is_empty() {
        out = "build".into();
    }
    out
}

pub struct FailureSnapshot {
    pub relative_dir: String,
    pub log_tail: String,
    pub exit_text: String,
}

/// Copy the server log slice and any Saved logs or crash dumps written for this
/// start. Paths are discovered from `install` and `log_dir`; nothing is joined
/// from the failure reason or the tag except a sanitized directory name.
pub fn snapshot_failure(
    work: &Path,
    install: &Path,
    log_dir: &Path,
    log_offset: u64,
    tag: &str,
    reason: &str,
    exit_code: Option<i32>,
) -> FailureSnapshot {
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let leaf = format!("{stamp}-{}", safe_tag(tag));
    let relative = format!("failed-starts/{leaf}");
    let dest = work.join("failed-starts").join(&leaf);
    let _ = fs::create_dir_all(&dest);
    let server_log = log_dir.join("server.log");
    let copied = copy_log_slice(
        &server_log,
        &dest.join("server-stdout.log"),
        log_offset,
        log_dir,
    );
    copy_saved_evidence(install, &dest.join("saved"));
    let exit_line = exit_text(exit_code);
    let _ = fs::write(dest.join("exit.txt"), format!("{exit_line}\n{reason}\n"));
    let tail_source = if copied.is_empty() {
        reason
    } else {
        copied.as_str()
    };
    FailureSnapshot {
        relative_dir: relative,
        log_tail: tail_lines_str(&redact(tail_source), 80),
        exit_text: exit_line,
    }
}

fn copy_log_slice(log: &Path, dest: &Path, offset: u64, allowed_dir: &Path) -> String {
    let Some(canon) = canonical_inside(allowed_dir, log) else {
        return String::new();
    };
    let Ok(mut file) = File::open(&canon) else {
        return String::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = offset.min(len);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = file.take(MAX_COPY_BYTES).read_to_end(&mut buf);
    let _ = fs::write(dest, &buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn copy_saved_evidence(install: &Path, dest_root: &Path) {
    let Some(install_canon) = install.canonicalize().ok() else {
        return;
    };
    for rel in launcher_core::resolve_preserved(install, Path::new("Saved")) {
        let saved = install.join(&rel);
        for sub in ["Logs", "Crashes"] {
            let src = saved.join(sub);
            if !src.is_dir() {
                continue;
            }
            let label = rel.to_string_lossy().replace(['/', '\\'], "_");
            copy_tree_inside(&src, &dest_root.join(sub).join(label), &install_canon);
        }
    }
}

fn copy_tree_inside(src: &Path, dest: &Path, root: &Path) {
    let Ok(entries) = fs::read_dir(src) else {
        return;
    };
    let _ = fs::create_dir_all(dest);
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            let name = entry.file_name();
            copy_tree_inside(&path, &dest.join(name), root);
            continue;
        }
        if !meta.is_file() || meta.len() > MAX_COPY_BYTES {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "log" | "txt" | "dmp" | "mdmp") {
            continue;
        }
        let Some(canon) = canonical_inside(root, &path) else {
            continue;
        };
        let name = entry.file_name();
        let _ = fs::copy(&canon, dest.join(name));
    }
}

pub fn tail_lines_str(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn read_scoped_log(
    install: &Path,
    work: &Path,
    log_dir: &Path,
    source: LogSource,
    failed_name: Option<&str>,
    lines: usize,
) -> Result<Value, String> {
    let lines = lines.clamp(1, 400);
    let (label, path) = match source {
        LogSource::Server => ("server.log", log_dir.join("server.log")),
        LogSource::Agent => ("agent", newest_under(log_dir, "log")?),
        LogSource::Ue => ("ue", newest_ue_log(install)?),
        LogSource::Failed => {
            let name = plain_name(failed_name.unwrap_or(""))?;
            let dir = work.join("failed-starts").join(name);
            ("failed", dir.join("server-stdout.log"))
        }
    };
    let allowed = match source {
        LogSource::Server | LogSource::Agent => log_dir,
        LogSource::Ue => install,
        LogSource::Failed => &work.join("failed-starts"),
    };
    let Some(canon) = canonical_inside(allowed, &path) else {
        return Err("that log is not in the agent scope".into());
    };
    let text = read_tail(&canon, lines);
    Ok(json!({
        "source": match source {
            LogSource::Server => "server",
            LogSource::Agent => "agent",
            LogSource::Ue => "ue",
            LogSource::Failed => "failed",
        },
        "name": label,
        "tail": redact(&text),
    }))
}

fn newest_under(dir: &Path, ext: &str) -> Result<PathBuf, String> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let entries = fs::read_dir(dir).map_err(|_| format!("no logs in {}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if best
            .as_ref()
            .map(|(when, _)| modified > *when)
            .unwrap_or(true)
        {
            best = Some((modified, path));
        }
    }
    best.map(|(_, path)| path)
        .ok_or_else(|| "no log file of that kind".into())
}

fn newest_ue_log(install: &Path) -> Result<PathBuf, String> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for rel in launcher_core::resolve_preserved(install, Path::new("Saved")) {
        let logs = install.join(rel).join("Logs");
        let Ok(entries) = fs::read_dir(&logs) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("log"))
                != Some(true)
            {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            if best
                .as_ref()
                .map(|(when, _)| modified > *when)
                .unwrap_or(true)
            {
                best = Some((modified, path));
            }
        }
    }
    best.map(|(_, path)| path)
        .ok_or_else(|| "no Unreal log under Saved/Logs".into())
}

fn read_tail(path: &Path, lines: usize) -> String {
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(256 * 1024);
    let _ = file.seek(SeekFrom::Start(start));
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    tail_lines_str(&String::from_utf8_lossy(&buf), lines)
}

#[derive(Debug, Clone)]
pub struct CrashHit {
    pub id: String,
    pub name: String,
    pub bytes: u64,
    pub too_large: bool,
}

fn crash_roots(install: &Path, work: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut roots = Vec::new();
    for rel in launcher_core::resolve_preserved(install, Path::new("Saved")) {
        roots.push((install.join(&rel).join("Crashes"), install.to_path_buf()));
    }
    roots.push((work.join("failed-starts"), work.join("failed-starts")));
    roots
}

pub fn list_crashes(install: &Path, work: &Path) -> Vec<CrashHit> {
    let mut found = Vec::new();
    for (dir, root) in crash_roots(install, work) {
        collect_dumps(&dir, &root, &mut found);
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found.truncate(100);
    found
}

fn collect_dumps(dir: &Path, root: &Path, out: &mut Vec<CrashHit>) {
    let Ok(root_canon) = root.canonicalize() else {
        return;
    };
    let mut stack = vec![dir.to_path_buf()];
    let mut depth = vec![0usize];
    while let Some(current) = stack.pop() {
        let here = depth.pop().unwrap_or(0);
        if here > 8 {
            continue;
        }
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
                depth.push(here + 1);
                continue;
            }
            let ext = path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "dmp" && ext != "mdmp" {
                continue;
            }
            let Some(canon) = canonical_inside(&root_canon, &path) else {
                continue;
            };
            let name = canon
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("dump.dmp")
                .to_string();
            let bytes = meta.len();
            out.push(CrashHit {
                id: stable_id(&canon.display().to_string()),
                name,
                bytes,
                too_large: bytes > MAX_DUMP_BYTES,
            });
        }
    }
}

pub fn crash_by_id(install: &Path, work: &Path, id: &str) -> Result<PathBuf, String> {
    let id = id.trim();
    if id.len() != 16 || !id.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err("unknown crash dump".into());
    }
    // The id is a hash of a canonical path this process discovered. It is not a path.
    for (dir, root) in crash_roots(install, work) {
        if let Some(path) = find_dump_path(&dir, &root, id) {
            return Ok(path);
        }
    }
    Err("unknown crash dump".into())
}

fn find_dump_path(dir: &Path, root: &Path, id: &str) -> Option<PathBuf> {
    let root_canon = root.canonicalize().ok()?;
    let mut stack = vec![dir.to_path_buf()];
    let mut depth = vec![0usize];
    while let Some(current) = stack.pop() {
        let here = depth.pop().unwrap_or(0);
        if here > 8 {
            continue;
        }
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
                depth.push(here + 1);
                continue;
            }
            let ext = path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "dmp" && ext != "mdmp" {
                continue;
            }
            let Some(canon) = canonical_inside(&root_canon, &path) else {
                continue;
            };
            if stable_id(&canon.display().to_string()) == id {
                return Some(canon);
            }
        }
    }
    None
}

fn stable_id(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

pub fn read_dump_chunk(path: &Path, offset: u64, limit: u64) -> Result<(Vec<u8>, u64), String> {
    let meta = fs::metadata(path).map_err(|err| err.to_string())?;
    let len = meta.len();
    if len > MAX_DUMP_BYTES {
        return Err("crash dump is larger than 50 MB and was not sent".into());
    }
    if offset > len {
        return Err("offset is past the end of the dump".into());
    }
    let limit = limit.clamp(1, MAX_CHUNK_BYTES).min(len - offset);
    let mut file = File::open(path).map_err(|err| err.to_string())?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|err| err.to_string())?;
    let mut buf = vec![0u8; limit as usize];
    file.read_exact(&mut buf).map_err(|err| err.to_string())?;
    Ok((buf, len))
}

/// Keep Application-log text that names the FnF server executable. A path in
/// the event that points at that executable must sit under one of `roots`.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub fn filter_application_events(text: &str, exe_name: &str, roots: &[PathBuf]) -> Vec<Value> {
    let needle = exe_name.trim();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut events = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.starts_with("Event[") && !current.is_empty() {
            if let Some(event) = keep_event(&current, needle, roots) {
                events.push(event);
            }
            current.clear();
        }
        current.push_str(line);
        current.push('\n');
    }
    if let Some(event) = keep_event(&current, needle, roots) {
        events.push(event);
    }
    events.truncate(50);
    events
}

fn keep_event(block: &str, exe_name: &str, roots: &[PathBuf]) -> Option<Value> {
    if !block
        .to_ascii_lowercase()
        .contains(&exe_name.to_ascii_lowercase())
    {
        return None;
    }
    let mut search = block;
    while let Some(start) = find_windows_path(search) {
        let path = &search[start..];
        let end = path.find(char::is_whitespace).unwrap_or(path.len());
        let path = &path[..end];
        if path
            .to_ascii_lowercase()
            .contains(&exe_name.to_ascii_lowercase())
            && !path_under_roots(path, roots)
        {
            return None;
        }
        search = &search[start + end..];
    }
    Some(json!({ "text": redact(block.trim()) }))
}

fn find_windows_path(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i].is_ascii_alphabetic()
            && bytes[i + 1] == b':'
            && (bytes[i + 2] == b'\\' || bytes[i + 2] == b'/')
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn path_under_roots(path: &str, roots: &[PathBuf]) -> bool {
    let norm = path.replace('/', "\\").to_ascii_lowercase();
    for root in roots {
        let root = root
            .display()
            .to_string()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase();
        if root.is_empty() {
            continue;
        }
        if norm == root || norm.starts_with(&(root.clone() + "\\")) {
            return true;
        }
    }
    false
}

pub fn application_events(exe_name: &str, roots: &[PathBuf]) -> Value {
    #[cfg(windows)]
    {
        match query_application_log() {
            Ok(text) => json!({
                "available": true,
                "source": "Application",
                "executable": exe_name,
                "events": filter_application_events(&text, exe_name, roots),
            }),
            Err(err) => json!({
                "available": false,
                "source": "Application",
                "executable": exe_name,
                "reason": err,
                "events": [],
            }),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (exe_name, roots);
        json!({
            "available": false,
            "source": "Application",
            "reason": "Application event log is read only on Windows, and only for the Friday Night Fights server process",
            "events": [],
        })
    }
}

#[cfg(windows)]
fn query_application_log() -> Result<String, String> {
    // Fixed arguments. The executable name is applied later, in process, as a filter.
    let output = std::process::Command::new("wevtutil")
        .args(["qe", "Application", "/c:200", "/rd:true", "/f:text"])
        .output()
        .map_err(|err| format!("could not read the Application log: {err}"))?;
    if !output.status.success() {
        return Err("Application log query failed".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn component_is_safe(path: &Path) -> bool {
    path.components().all(|component| match component {
        Component::Normal(_) | Component::CurDir => true,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_addresses_and_secrets_and_keeps_versions() {
        let text = "build v0.3.12 token=abcd key: hunter2 host 203.0.113.10 and 127.0.0.1 ::1 fe80::1 2001:db8:85a3::8a2e:370:7334 at 03:00:00";
        let clean = redact(text);
        assert!(clean.contains("v0.3.12"), "{clean}");
        assert!(clean.contains("03:00:00"), "{clean}");
        assert!(clean.contains("127.0.0.1"), "{clean}");
        assert!(!clean.contains("abcd"), "{clean}");
        assert!(!clean.contains("hunter2"), "{clean}");
        assert!(!clean.contains("203.0.113.10"), "{clean}");
        assert!(!clean.contains("fe80"), "{clean}");
        assert!(!clean.contains("2001:db8"), "{clean}");
    }

    #[test]
    fn rejects_path_names() {
        assert!(plain_name("..").is_err());
        assert!(plain_name("../etc/passwd").is_err());
        assert!(plain_name("C:\\FNF\\x").is_err());
        assert!(plain_name("ok-name.log").is_ok());
        assert!(parse_log_source("../../etc/passwd").is_err());
        assert_eq!(parse_log_source("server"), Ok(LogSource::Server));
    }

    #[test]
    fn snapshot_and_crash_id_stay_inside_the_tree() {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("install");
        let outside = temp.path().join("outside");
        let log_dir = temp.path().join("logs");
        fs::create_dir_all(install.join("Saved/Logs")).unwrap();
        fs::create_dir_all(install.join("Saved/Crashes/crash1")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        fs::write(
            install.join("Saved/Logs/game.log"),
            b"Purgatory refuses UDP 7777\n",
        )
        .unwrap();
        fs::write(install.join("Saved/Crashes/crash1/crash.dmp"), b"DMP").unwrap();
        fs::write(outside.join("secret.dmp"), b"NO").unwrap();
        fs::write(
            log_dir.join("server.log"),
            b"stdout token=hide-me\nbad-start\n",
        )
        .unwrap();
        // A symlink to the outside dump must not be collected.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.join("secret.dmp"),
                install.join("Saved/Crashes/linked.dmp"),
            )
            .unwrap();
        }
        let snap = snapshot_failure(
            temp.path(),
            &install,
            &log_dir,
            0,
            "v0.10.0",
            "process exited",
            Some(1),
        );
        let dir = temp.path().join(&snap.relative_dir);
        assert!(dir.join("exit.txt").is_file());
        assert!(dir.join("server-stdout.log").is_file());
        let saved = fs::read_to_string(dir.join("server-stdout.log")).unwrap();
        assert!(
            saved.contains("token=hide-me"),
            "on-disk copy keeps the raw log"
        );
        assert!(!snap.log_tail.contains("hide-me"), "{}", snap.log_tail);
        let crashes = list_crashes(&install, temp.path());
        assert!(crashes.iter().any(|hit| hit.name == "crash.dmp"));
        assert!(crashes.iter().all(|hit| hit.name != "secret.dmp"));
        assert!(crashes.iter().all(|hit| hit.name != "linked.dmp"));
        let id = crashes
            .iter()
            .find(|hit| hit.name == "crash.dmp")
            .unwrap()
            .id
            .clone();
        let path = crash_by_id(&install, temp.path(), &id).unwrap();
        assert!(path.starts_with(install.canonicalize().unwrap()));
        assert!(crash_by_id(&install, temp.path(), "../secret.dmp").is_err());
        assert!(crash_by_id(&install, temp.path(), "not-a-real-id!!").is_err());
    }

    #[test]
    fn event_filter_keeps_only_the_fnf_image_under_the_root() {
        let text = "\
Event[0]:
  Source: Application Error
  Faulting application name: FridayNightFightsServer.exe
  Faulting application path: C:\\FNF\\Server\\FridayNightFightsServer.exe
Event[1]:
  Source: Application Error
  Faulting application name: OtherGame.exe
  Faulting application path: D:\\Other\\OtherGame.exe
Event[2]:
  Source: Application Error
  Faulting application name: FridayNightFightsServer.exe
  Faulting application path: D:\\Other\\FridayNightFightsServer.exe
";
        let roots = vec![PathBuf::from(r"C:\FNF")];
        let kept = filter_application_events(text, "FridayNightFightsServer.exe", &roots);
        assert_eq!(kept.len(), 1, "{kept:?}");
        let body = kept[0]["text"].as_str().unwrap();
        assert!(body.contains("C:\\FNF\\Server"));
        assert!(!body.contains("OtherGame"));
        assert!(!body.contains("D:\\Other"));
    }
}
