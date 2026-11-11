//! One-time admin setup. It configures the uXaServerAgent service and writes
//! `agent.json` only when that file is inside the FnF root (C:\FNF on the host).

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

pub const FNF_ROOT: &str = r"C:\FNF";
pub const SERVICE_NAME: &str = "uXaServerAgent";
pub const PINNED_OWNER: &str = "uXa-Sloppy";
pub const PINNED_REPO: &str = "FridayNightFights-Releases";

const SECRET_KEYS: &[&str] = &[
    "secret",
    "token",
    "githubToken",
    "github_token",
    "agentSecret",
    "password",
];

/// Fixed service commands. The service name is the only one they name, and
/// none of them carry a path from the caller.
pub fn service_commands() -> Vec<Vec<String>> {
    vec![
        vec![
            "sc.exe".into(),
            "config".into(),
            SERVICE_NAME.into(),
            "start=".into(),
            "auto".into(),
            "obj=".into(),
            "LocalSystem".into(),
        ],
        vec![
            "sc.exe".into(),
            "failure".into(),
            SERVICE_NAME.into(),
            "reset=".into(),
            "86400".into(),
            "actions=".into(),
            "restart/5000/restart/5000/restart/5000".into(),
        ],
        vec![
            "sc.exe".into(),
            "failureflag".into(),
            SERVICE_NAME.into(),
            "1".into(),
        ],
        vec!["sc.exe".into(), "stop".into(), SERVICE_NAME.into()],
        vec!["sc.exe".into(), "start".into(), SERVICE_NAME.into()],
    ]
}

fn has_secret_key(value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    obj.keys().any(|key| {
        SECRET_KEYS
            .iter()
            .any(|secret| key.eq_ignore_ascii_case(secret))
    })
}

pub fn write_on_demand_config(path: &Path) -> Result<(), String> {
    let text =
        fs::read_to_string(path).map_err(|err| format!("could not read agent.json: {err}"))?;
    let mut value: Value = serde_json::from_str(&text)
        .map_err(|err| format!("agent.json is not valid JSON: {err}"))?;
    if has_secret_key(&value) {
        return Err("agent.json contains a secret field; it was not rewritten".into());
    }
    let obj = value
        .as_object_mut()
        .ok_or_else(|| "agent.json must be a JSON object".to_string())?;
    obj.insert("owner".into(), json!(PINNED_OWNER));
    obj.insert("repo".into(), json!(PINNED_REPO));
    let auto = obj.entry("autoUpdate").or_insert_with(|| json!({}));
    let auto = auto
        .as_object_mut()
        .ok_or_else(|| "autoUpdate must be an object".to_string())?;
    auto.insert("updateMode".into(), json!("on-demand"));
    auto.insert("onFailedStart".into(), json!("stay"));
    auto.insert("checkIntervalMinutes".into(), json!(10));
    let pretty = serde_json::to_string_pretty(&value).map_err(|err| err.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, format!("{pretty}\n")).map_err(|err| err.to_string())?;
    fs::rename(&tmp, path).map_err(|err| err.to_string())
}

fn normalized(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        return path.canonicalize().map_err(|err| err.to_string());
    }
    let parent = path.parent().unwrap_or(Path::new("."));
    if parent.as_os_str().is_empty() {
        return Err("path has no parent".into());
    }
    let parent = if parent.exists() {
        parent.canonicalize().map_err(|err| err.to_string())?
    } else {
        return Err(format!("{} does not exist", parent.display()));
    };
    let name = path
        .file_name()
        .ok_or_else(|| "path has no file name".to_string())?;
    if name == "." || name == ".." {
        return Err("refusing a relative path".into());
    }
    Ok(parent.join(name))
}

pub fn ensure_under_root(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let root = normalized(root)?;
    let path = normalized(path)?;
    if path == root || path.starts_with(&root) {
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err("refusing a path that climbs out of C:\\FNF".into());
        }
        return Ok(path);
    }
    Err(format!(
        "refusing to touch {}; setup-fnf only edits the uXaServerAgent service and files under {}",
        path.display(),
        root.display()
    ))
}

pub fn locate_config(root: &Path) -> Result<PathBuf, String> {
    if let Some(from_env) = std::env::var("UXA_AGENT_CONFIG")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return ensure_under_root(root, Path::new(&from_env));
    }
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("agent.json"));
        }
    }
    candidates.push(root.join("agent.json"));
    for candidate in candidates {
        if candidate.is_file() {
            return ensure_under_root(root, &candidate);
        }
    }
    Err(format!(
        "no agent.json under {}. setup-fnf does not create one outside that folder.",
        root.display()
    ))
}

/// Write the on-demand config and, when `run_service` is set, apply the fixed
/// service commands. `run_service` is for the Windows admin command.
pub fn setup_fnf_at(root: &Path, config_path: &Path, run_service: bool) -> Result<String, String> {
    let config_path = ensure_under_root(root, config_path)?;
    if !config_path.is_file() {
        return Err(format!("no agent.json at {}", config_path.display()));
    }
    write_on_demand_config(&config_path)?;
    let mut lines = vec![
        "uXa Server Agent setup-fnf".to_string(),
        format!("service: {SERVICE_NAME}"),
        "account: LocalSystem".into(),
        "failure: restart".into(),
        format!("config: {}", config_path.display()),
        "updateMode: on-demand".into(),
        "onFailedStart: stay".into(),
        format!("repo: {PINNED_OWNER}/{PINNED_REPO}"),
        "scope: uXaServerAgent service and C:\\FNF only".into(),
    ];
    if run_service {
        for command in service_commands() {
            let status = Command::new(&command[0])
                .args(&command[1..])
                .status()
                .map_err(|err| format!("{} failed to start: {err}", command[0]))?;
            lines.push(format!("{} -> {status}", command.join(" ")));
        }
    } else {
        lines.push("service changes: planned, not applied by this call".into());
    }
    Ok(lines.join("\n"))
}

pub fn setup_fnf() -> Result<String, String> {
    let root = PathBuf::from(FNF_ROOT);
    if !root.is_dir() {
        return Err(
            "C:\\FNF does not exist. setup-fnf will not create folders or edit any other path."
                .into(),
        );
    }
    let config = locate_config(&root)?;
    setup_fnf_at(&root, &config, cfg!(windows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_commands_name_only_the_fnf_agent() {
        let commands = service_commands();
        assert!(commands.len() >= 4);
        for command in &commands {
            assert_eq!(command[0], "sc.exe");
            assert_eq!(
                command
                    .iter()
                    .filter(|arg| arg.as_str() == SERVICE_NAME)
                    .count(),
                1,
                "{command:?}"
            );
            assert!(
                command.iter().all(|arg| {
                    !arg.contains("..")
                        && !arg.contains('\\')
                        && (arg == "restart/5000/restart/5000/restart/5000" || !arg.contains('/'))
                }),
                "{command:?}"
            );
            let joined = command.join(" ").to_ascii_lowercase();
            assert!(!joined.contains("othergame"));
            assert!(!joined.contains("cmd.exe"));
            assert!(!joined.contains("powershell"));
        }
        let flat = commands
            .iter()
            .flat_map(|command| command.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(flat.contains("LocalSystem"));
        assert!(flat.contains("restart/5000/restart/5000/restart/5000"));
        assert!(flat.contains("start= auto") || flat.contains("start="));
    }

    #[test]
    fn setup_writes_only_a_config_inside_the_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("FNF");
        fs::create_dir_all(&root).unwrap();
        let config = root.join("agent.json");
        fs::write(
            &config,
            r#"{"installDir":"C:\\FNF\\Server","listen":"0.0.0.0:8788","port":7777,"owner":"uXa-Sloppy","repo":"old","autoUpdate":{"enabled":true,"dailyRestart":true,"serverEnabled":true,"restartTime":"03:00"}}"#,
        )
        .unwrap();
        let outside = temp.path().join("other-agent.json");
        fs::write(
            &outside,
            r#"{"installDir":"D:\\Other","listen":"0.0.0.0:1"}"#,
        )
        .unwrap();
        let err = setup_fnf_at(&root, &outside, false).unwrap_err();
        assert!(err.contains("refusing"), "{err}");
        assert_eq!(
            fs::read_to_string(&outside).unwrap(),
            r#"{"installDir":"D:\\Other","listen":"0.0.0.0:1"}"#
        );

        let report = setup_fnf_at(&root, &config, false).unwrap();
        assert!(report.contains("updateMode: on-demand"), "{report}");
        assert!(report.contains("uXaServerAgent"), "{report}");
        assert!(!report.contains("secret"));
        let value: Value = serde_json::from_str(&fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(value["repo"], "FridayNightFights-Releases");
        assert_eq!(value["owner"], "uXa-Sloppy");
        assert_eq!(value["listen"], "0.0.0.0:8788");
        assert_eq!(value["port"], 7777);
        assert_eq!(value["installDir"], "C:\\FNF\\Server");
        assert_eq!(value["autoUpdate"]["updateMode"], "on-demand");
        assert_eq!(value["autoUpdate"]["onFailedStart"], "stay");
        assert_eq!(value["autoUpdate"]["checkIntervalMinutes"], 10);
        assert_eq!(value["autoUpdate"]["enabled"], true);
        assert!(
            !outside.exists()
                || fs::read_to_string(&outside)
                    .unwrap()
                    .contains("D:\\\\Other")
                || fs::read_to_string(&outside).unwrap().contains("D:\\Other")
        );
    }

    #[test]
    fn setup_refuses_a_secret_field_without_echoing_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("FNF");
        fs::create_dir_all(&root).unwrap();
        let config = root.join("agent.json");
        fs::write(
            &config,
            r#"{"installDir":"C:\\FNF\\Server","secret":"do-not-print"}"#,
        )
        .unwrap();
        let err = setup_fnf_at(&root, &config, false).unwrap_err();
        assert!(err.contains("secret field"), "{err}");
        assert!(!err.contains("do-not-print"), "{err}");
    }
}
