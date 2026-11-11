//! Silent install of prerequisite packages that shipped inside a hash-checked
//! release. The executable path always comes from a walk of the install tree.
//! A request body cannot name an installer.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const INSTALL_ARGS: &[&str] = &["/install", "/quiet", "/norestart"];
pub const INSTALLER_NAMES: &[&str] = &["vc_redist.x64.exe", "UEPrereqSetup_x64.exe"];
const MAX_DEPTH: usize = 8;
const HASH_FILE: &str = "prereq-hashes.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedInstall {
    pub relative: String,
    pub args: &'static [&'static str],
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HashFile {
    tag: String,
    files: BTreeMap<String, String>,
}

pub fn is_loader_failure(code: Option<i32>) -> bool {
    matches!(
        code.map(|code| code as u32),
        Some(0xC0000135 | 0xC000007B | 0xC0000139)
    )
}

pub fn should_retry(code: Option<i32>, already_retried: bool) -> bool {
    !already_retried && is_loader_failure(code)
}

/// Installers found by file name under `root`. Symlinks are ignored. A file
/// whose canonical path leaves `root` is not returned.
pub fn discover(root: &Path) -> Vec<PathBuf> {
    let Ok(root_canon) = root.canonicalize() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    walk(&root_canon, &root_canon, 0, &mut found);
    found.sort();
    found
}

fn walk(dir: &Path, root: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if meta.is_dir() {
            if name == "Saved" || name.starts_with('.') {
                continue;
            }
            walk(&path, root, depth + 1, out);
            continue;
        }
        if !INSTALLER_NAMES
            .iter()
            .any(|want| name.eq_ignore_ascii_case(want))
        {
            continue;
        }
        let Ok(canon) = path.canonicalize() else {
            continue;
        };
        if canon.starts_with(root) {
            out.push(canon);
        }
    }
}

fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let root = root.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    let rel = path.strip_prefix(&root).ok()?;
    if !crate::diag::component_is_safe(rel) {
        return None;
    }
    Some(rel.to_string_lossy().replace('\\', "/"))
}

pub fn record_hashes(work: &Path, tag: &str, root: &Path) -> Result<(), String> {
    let mut files = BTreeMap::new();
    for path in discover(root) {
        let Some(rel) = relative_to(root, &path) else {
            continue;
        };
        let sha = launcher_core::file_sha256(&path).map_err(|err| err.to_string())?;
        files.insert(rel, sha);
    }
    if files.is_empty() {
        return Ok(());
    }
    let body = HashFile {
        tag: tag.to_string(),
        files,
    };
    fs::create_dir_all(work).map_err(|err| err.to_string())?;
    let text = serde_json::to_string_pretty(&body).map_err(|err| err.to_string())?;
    let path = work.join(HASH_FILE);
    let tmp = work.join(format!("{HASH_FILE}.tmp"));
    fs::write(&tmp, text).map_err(|err| err.to_string())?;
    fs::rename(&tmp, path).map_err(|err| err.to_string())
}

/// Plans the fixed `/install /quiet /norestart` command for each installer
/// whose bytes still match the hash recorded from the verified zip.
pub fn verify_and_plan(work: &Path, tag: &str, root: &Path) -> Result<Vec<PlannedInstall>, String> {
    let path = work.join(HASH_FILE);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let recorded: HashFile = serde_json::from_str(&text).map_err(|err| err.to_string())?;
    if recorded.tag != tag {
        return Ok(Vec::new());
    }
    let mut planned = Vec::new();
    for (rel, sha) in &recorded.files {
        if rel.contains("..") || rel.starts_with('/') || rel.contains(':') {
            return Err(format!("refusing prerequisite path {rel}"));
        }
        let name = Path::new(rel)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if !INSTALLER_NAMES
            .iter()
            .any(|want| name.eq_ignore_ascii_case(want))
        {
            return Err(format!("refusing unexpected prerequisite {rel}"));
        }
        let full = root.join(rel);
        let Some(canon) = full.canonicalize().ok().filter(|canon| {
            root.canonicalize()
                .ok()
                .is_some_and(|root| canon.starts_with(&root))
        }) else {
            return Err(format!("prerequisite {rel} is not inside the install"));
        };
        let actual = launcher_core::file_sha256(&canon).map_err(|err| err.to_string())?;
        if !actual.eq_ignore_ascii_case(sha) {
            return Err(format!(
                "prerequisite {rel} does not match the hash recorded from the release; it was not run"
            ));
        }
        planned.push(PlannedInstall {
            relative: rel.clone(),
            args: INSTALL_ARGS,
        });
    }
    Ok(planned)
}

fn marker_path(work: &Path, tag: &str) -> PathBuf {
    let safe: String = tag
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '.' || *ch == '-' || *ch == '_')
        .take(40)
        .collect();
    work.join(format!("prereqs-applied-{safe}.txt"))
}

/// Run hash-checked bundled installers. On Windows the process is the
/// installer itself with `/install /quiet /norestart`. This never reboots.
pub fn install_bundled(
    install: &Path,
    work: &Path,
    tag: &str,
    force: bool,
) -> Result<String, String> {
    let planned = verify_and_plan(work, tag, install)?;
    if planned.is_empty() {
        return Ok(String::new());
    }
    let marker = marker_path(work, tag);
    if marker.is_file() && !force {
        return Ok(format!("bundled prerequisites already applied for {tag}"));
    }
    #[cfg(not(windows))]
    {
        let _ = (force, marker);
        return Ok(format!(
            "bundled prerequisites present ({}); silent install runs on Windows only",
            planned.len()
        ));
    }
    #[cfg(windows)]
    {
        let mut notes = Vec::new();
        let mut failed = false;
        for item in &planned {
            let path = install.join(&item.relative);
            let actual = launcher_core::file_sha256(&path).map_err(|err| err.to_string())?;
            let expected = fs::read_to_string(work.join(HASH_FILE)).unwrap_or_default();
            if !expected
                .to_ascii_lowercase()
                .contains(&actual.to_ascii_lowercase())
            {
                notes.push(format!("{} hash mismatch; not run", item.relative));
                failed = true;
                continue;
            }
            let output = std::process::Command::new(&path)
                .args(item.args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map_err(|err| format!("could not start {}: {err}", item.relative))?;
            let code = output.code().unwrap_or(-1);
            notes.push(format!("{} exit {code}", item.relative));
            if code == 3010 {
                notes.push(
                    "prerequisite installer asked for a reboot; the agent did not reboot".into(),
                );
            }
            if !matches!(code, 0 | 1638 | 3010) {
                failed = true;
            }
        }
        if !failed {
            let _ = fs::create_dir_all(work);
            fs::write(&marker, notes.join("\n")).map_err(|err| err.to_string())?;
        }
        Ok(notes.join("; "))
    }
}

pub fn prereq_report(install: &Path, work: &Path) -> serde_json::Value {
    let bundled: Vec<String> = discover(install)
        .iter()
        .filter_map(|path| relative_to(install, path))
        .collect();
    let hashes = fs::read_to_string(work.join(HASH_FILE)).unwrap_or_default();
    serde_json::json!({
        "platform": std::env::consts::OS,
        "bundled": bundled,
        "hashesRecorded": !hashes.is_empty(),
        "vcRuntime": vc_runtime_version(),
        "installerArgs": INSTALL_ARGS,
    })
}

fn vc_runtime_version() -> Option<String> {
    #[cfg(windows)]
    {
        let output = std::process::Command::new("reg")
            .args([
                "query",
                r"HKLM\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64",
                "/v",
                "Version",
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            if parts.next() == Some("Version") {
                return parts.nth(1).map(|value| value.to_string());
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loader_codes_retry_once() {
        assert!(is_loader_failure(Some(0xC0000135u32 as i32)));
        assert!(is_loader_failure(Some(0xC000007Bu32 as i32)));
        assert!(is_loader_failure(Some(0xC0000139u32 as i32)));
        assert!(!is_loader_failure(Some(1)));
        assert!(!is_loader_failure(None));
        assert!(should_retry(Some(0xC0000135u32 as i32), false));
        assert!(!should_retry(Some(0xC0000135u32 as i32), true));
        assert!(!should_retry(Some(1), false));
    }

    #[test]
    fn discovers_only_bundled_installers_inside_the_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("install");
        let nested = root.join("Engine/Extras/Redist/en-us");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("vc_redist.x64.exe"), b"VC-BYTES").unwrap();
        fs::write(nested.join("UEPrereqSetup_x64.exe"), b"UE-BYTES").unwrap();
        fs::write(nested.join("note.txt"), b"no").unwrap();
        let outside = temp.path().join("vc_redist.x64.exe");
        fs::write(&outside, b"OUT").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("vc_redist.x64.exe")).unwrap();
        }
        let found = discover(&root);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found
            .iter()
            .all(|path| path.starts_with(root.canonicalize().unwrap())));
        record_hashes(temp.path(), "v0.10.1", &root).unwrap();
        let planned = verify_and_plan(temp.path(), "v0.10.1", &root).unwrap();
        assert_eq!(planned.len(), 2);
        for item in &planned {
            assert_eq!(item.args, &["/install", "/quiet", "/norestart"]);
            assert!(!item.relative.contains(".."));
        }
        fs::write(nested.join("vc_redist.x64.exe"), b"TAMPERED").unwrap();
        let err = verify_and_plan(temp.path(), "v0.10.1", &root).unwrap_err();
        assert!(err.contains("does not match"), "{err}");
    }

    #[test]
    fn a_hash_file_cannot_point_outside_the_install() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("install");
        fs::create_dir_all(&root).unwrap();
        let evil = serde_json::json!({
            "tag": "v1",
            "files": { "../vc_redist.x64.exe": "abcd" }
        });
        fs::write(temp.path().join(HASH_FILE), evil.to_string()).unwrap();
        let err = verify_and_plan(temp.path(), "v1", &root).unwrap_err();
        assert!(err.contains("refusing"), "{err}");
    }
}
