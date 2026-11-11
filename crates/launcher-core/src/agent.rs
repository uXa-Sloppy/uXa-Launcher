//! Signed calls from the launcher to a uXa Server Agent.
//! The admin key is an argument, never read from settings.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::sign::sign_request;
use crate::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    pub version: String,
    pub online: bool,
    pub uptime_secs: u64,
    pub player_count: Option<u32>,
    pub port: u16,
    pub phase: String,
    /// uXa Server Agent build. Empty when an older agent did not send it.
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub last_check_at: String,
    #[serde(default)]
    pub last_check_result: String,
    #[serde(default)]
    pub last_update_at: String,
    #[serde(default)]
    pub last_update_result: String,
    /// Server build downloaded and waiting for the daily restart window.
    #[serde(default)]
    pub staged_version: String,
    /// Next time the game server may be restarted, RFC3339 with the zone offset.
    /// Empty in 0.3.12: nothing restarts on a clock.
    #[serde(default)]
    pub next_restart_at: String,
    /// `on-demand` from 0.3.12. Game installs happen only on a signed command.
    #[serde(default)]
    pub update_mode: String,
    /// Tag of the build kept at `{install}.previous`, if one is on disk.
    #[serde(default)]
    pub previous_version: String,
    /// Set when a new build failed to stay up. The agent does not roll it back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_start: Option<FailedStart>,
    /// Written after this process binds its listener, including after a self-update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_health: Option<AgentHealth>,
    /// Minutes between report-only GitHub polls. Always 5–15.
    #[serde(default)]
    pub check_interval_minutes: u64,
}

/// Evidence from a game start that exited before it was healthy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FailedStart {
    pub tag: String,
    pub at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub exit_text: String,
    #[serde(default)]
    pub log_tail: String,
    /// Relative to the agent work directory. Not a path the caller can choose.
    #[serde(default)]
    pub artifact_dir: String,
}

/// Post-start report for the agent process itself (not the game server).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentHealth {
    pub ok: bool,
    pub at: String,
    pub agent_version: String,
    pub update_mode: String,
    pub listening: bool,
    #[serde(default)]
    pub pending_install_cancelled: bool,
    #[serde(default)]
    pub cancelled_staged_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentReleaseSummary {
    pub tag: String,
    pub name: String,
    pub prerelease: bool,
    pub asset_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentLogLine {
    pub id: u64,
    pub at: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAction {
    pub ok: bool,
    pub message: String,
    pub status: AgentStatus,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn error_message(status: u16, body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(message) = value.get("error").and_then(|v| v.as_str()) {
            if !message.trim().is_empty() {
                return message.trim().to_string();
            }
        }
    }
    let detail = body
        .chars()
        .take(180)
        .collect::<String>()
        .replace('\n', " ");
    if detail.is_empty() {
        format!("Server agent returned {status}")
    } else {
        format!("Server agent returned {status}: {detail}")
    }
}

/// `path_and_query` begins with `/`. Only the path is covered by the signature.
pub async fn agent_exchange(
    client: &reqwest::Client,
    base: &str,
    secret: &str,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> Result<String> {
    let base = base.trim().trim_end_matches('/');
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err(Error::msg(
            "Server agent address must start with https:// or http://.",
        ));
    }
    let path = path_and_query.split('?').next().unwrap_or(path_and_query);
    if !path.starts_with('/') {
        return Err(Error::msg("agent path must start with /"));
    }
    let timestamp = unix_now().to_string();
    let signature = sign_request(secret, &timestamp, method, path, body)?;
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| Error::msg("unsupported agent method"))?;
    let response = client
        .request(method, format!("{base}{path_and_query}"))
        .header("X-Uxa-Timestamp", timestamp)
        .header("X-Uxa-Signature", signature)
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .map_err(|err| Error::msg(format!("Could not reach the server agent at {base}: {err}")))?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if status == 401 {
        return Err(Error::msg("Admin key was rejected by the server agent."));
    }
    if !(200..300).contains(&status) {
        return Err(Error::msg(error_message(status, &text)));
    }
    Ok(text)
}
