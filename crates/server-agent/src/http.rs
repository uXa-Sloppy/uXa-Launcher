//! Authenticated API. Every request needs a fresh HMAC signature.
//! TLS is used when cert and key paths are set; otherwise the listener is HTTP
//! and the signature is what authenticates the caller.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use launcher_core::{verify_request, AgentAction, AgentLogLine};
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::runtime::Agent;

#[derive(Deserialize)]
struct WipeBody {
    #[serde(default)]
    confirm: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct InviteBody {
    key_sha256: String,
    note: String,
}

#[derive(Deserialize)]
struct DeployBody {
    #[serde(default)]
    tag: String,
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message }))).into_response()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

fn query_string(uri: &Uri, key: &str) -> String {
    let Some(query) = uri.query() else {
        return String::new();
    };
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some(key) {
            return percent_decode(parts.next().unwrap_or(""));
        }
    }
    String::new()
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn public_config(value: &serde_json::Value) -> serde_json::Value {
    let mut copy = value.clone();
    if let Some(obj) = copy.as_object_mut() {
        obj.retain(|key, _| {
            !matches!(
                key.to_ascii_lowercase().as_str(),
                "secret" | "token" | "password" | "githubtoken" | "github_token" | "agentsecret"
            )
        });
    }
    copy
}

fn query_u64(uri: &Uri, key: &str) -> u64 {
    let Some(query) = uri.query() else {
        return 0;
    };
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some(key) {
            return parts.next().unwrap_or("0").parse().unwrap_or(0);
        }
    }
    0
}

/// Public prefix the agent proxies to its local account server (127.0.0.1:<accountServer.port>).
pub const ACCOUNT_PROXY_PREFIX: &str = "/fnf-accounts";
const ACCOUNT_MAX_BODY: usize = 4096;
const SIGNUPS_PER_IP_PER_MIN: usize = 6;
const SIGNUPS_PER_MIN: usize = 60;
const HEALTH_PER_MIN: usize = 120;

/// `/fnf-accounts/v1/signup`, `/fnf-accounts/v1/health`, and the form game clients up to
/// 0.10.0 send (`/fnf-accounts:<port>/v1/...`, because they build `http://<spec>/v1/signup`
/// from `-FNFAccountServer=<host>:<port>/fnf-accounts:<port>`). Anything else: None.
pub fn account_proxy_target(path: &str) -> Option<&'static str> {
    let rest = path.strip_prefix(ACCOUNT_PROXY_PREFIX)?;
    let rest = match rest.strip_prefix(':') {
        Some(after) => {
            let end = after.find('/').unwrap_or(after.len());
            if end == 0 || end > 5 || !after[..end].bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            &after[end..]
        }
        None => rest,
    };
    match rest {
        "/v1/signup" => Some("/v1/signup"),
        "/v1/health" => Some("/v1/health"),
        _ => None,
    }
}

struct RateWindow {
    per_ip: std::collections::HashMap<String, Vec<std::time::Instant>>,
    signups: Vec<std::time::Instant>,
    health: Vec<std::time::Instant>,
}

fn rate_window() -> &'static std::sync::Mutex<RateWindow> {
    static W: std::sync::OnceLock<std::sync::Mutex<RateWindow>> = std::sync::OnceLock::new();
    W.get_or_init(|| {
        std::sync::Mutex::new(RateWindow {
            per_ip: Default::default(),
            signups: Vec::new(),
            health: Vec::new(),
        })
    })
}

/// Sliding one-minute limits: per client IP and overall for sign-ups, overall for health checks.
fn rate_allow(ip: &str, signup: bool) -> bool {
    let now = std::time::Instant::now();
    let minute = std::time::Duration::from_secs(60);
    let Ok(mut w) = rate_window().lock() else {
        return false;
    };
    let fresh = |v: &mut Vec<std::time::Instant>| v.retain(|t| now.duration_since(*t) < minute);
    if !signup {
        fresh(&mut w.health);
        if w.health.len() >= HEALTH_PER_MIN {
            return false;
        }
        w.health.push(now);
        return true;
    }
    fresh(&mut w.signups);
    if w.per_ip.len() > 4096 {
        w.per_ip
            .retain(|_, v| v.last().map_or(false, |t| now.duration_since(*t) < minute));
    }
    let mine = w.per_ip.entry(ip.to_string()).or_default();
    mine.retain(|t| now.duration_since(*t) < minute);
    if mine.len() >= SIGNUPS_PER_IP_PER_MIN {
        return false;
    }
    mine.push(now);
    if w.signups.len() >= SIGNUPS_PER_MIN {
        return false;
    }
    w.signups.push(now);
    true
}

fn client_ip(headers: &HeaderMap) -> String {
    // Behind a Cloudflare tunnel every request comes from cloudflared on loopback; Cloudflare
    // sets CF-Connecting-IP. Direct callers could fake it, but the overall limit still holds.
    let cf = header(headers, "cf-connecting-ip").trim();
    if !cf.is_empty() {
        return cf.chars().take(64).collect();
    }
    let xff = header(headers, "x-forwarded-for")
        .split(',')
        .next()
        .unwrap_or("")
        .trim();
    if !xff.is_empty() {
        return xff.chars().take(64).collect();
    }
    "direct".into()
}

fn account_json(status: StatusCode, error: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "ok": false, "error": error, "message": message })),
    )
        .into_response()
}

/// Public, unsigned: forwards invite sign-ups to the local account server. Bodies (invite keys,
/// passwords) are passed through untouched and never logged.
async fn account_proxy(
    agent: &Agent,
    method: &Method,
    target: &'static str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let signup = target == "/v1/signup";
    let allowed = if signup {
        *method == Method::POST
    } else {
        *method == Method::GET
    };
    if !allowed {
        return account_json(StatusCode::METHOD_NOT_ALLOWED, "not_found", "Not found.");
    }
    if body.len() > ACCOUNT_MAX_BODY {
        return account_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            "bad_request",
            "Request too large.",
        );
    }
    let cfg = &agent.config.account_server;
    if !cfg.enabled {
        return account_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Sign-ups are not enabled on this server.",
        );
    }
    let ip = client_ip(headers);
    if !rate_allow(&ip, signup) {
        if signup {
            agent.log("account proxy: sign-up rate limit hit".to_string());
        }
        return account_json(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many sign-up attempts. Try again in a minute.",
        );
    }
    let url = format!("http://127.0.0.1:{}{}", cfg.port, target);
    let request = if signup {
        agent
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body.to_vec())
    } else {
        agent.client.get(&url)
    };
    let sent = tokio::time::timeout(std::time::Duration::from_secs(10), request.send()).await;
    let response = match sent {
        Ok(Ok(r)) => r,
        _ => {
            if signup {
                agent.log("account proxy: account server did not answer".to_string());
            }
            return account_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "Account server is offline. Try again shortly.",
            );
        }
    };
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = match response.bytes().await {
        Ok(b) if b.len() <= 64 * 1024 => b,
        _ => {
            return account_json(
                StatusCode::BAD_GATEWAY,
                "unavailable",
                "Account server reply was unreadable.",
            )
        }
    };
    if signup {
        agent.log(format!(
            "account proxy: sign-up -> HTTP {}",
            status.as_u16()
        ));
    }
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response()
}

async fn dispatch(
    State(agent): State<Arc<Agent>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path();
    if path.starts_with(ACCOUNT_PROXY_PREFIX) {
        return match account_proxy_target(path) {
            Some(target) => account_proxy(&agent, &method, target, &headers, body).await,
            None => account_json(StatusCode::NOT_FOUND, "not_found", "Not found."),
        };
    }
    let timestamp = header(&headers, "x-uxa-timestamp");
    let signature = header(&headers, "x-uxa-signature");
    if let Err(err) = verify_request(
        &agent.config.secret,
        timestamp,
        method.as_str(),
        path,
        &body,
        signature,
        unix_now(),
    ) {
        agent.log(format!("rejected {method} {path}: {err}"));
        return json_error(
            StatusCode::UNAUTHORIZED,
            "Admin key was rejected by the server agent.",
        );
    }
    agent.log(format!("signed {method} {path}"));

    let result = match (method.as_str(), path) {
        ("GET", "/v1/status") => Ok(Reply::Status(agent.status().await)),
        ("GET", "/v1/releases") => agent.releases().await.map(Reply::Releases),
        ("GET", "/v1/serverlog") => {
            return axum::Json(agent.server_log()).into_response();
        }
        ("GET", "/v1/logs") => {
            let after = query_u64(&uri, "after");
            Ok(Reply::Logs(agent.logs_after(after)))
        }
        ("POST", "/v1/deploy") => {
            let tag = if body.is_empty() {
                String::new()
            } else {
                match serde_json::from_slice::<DeployBody>(&body) {
                    Ok(body) => body.tag,
                    Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
                }
            };
            agent.deploy(&tag).await.map(|status| {
                Reply::Action(AgentAction {
                    ok: true,
                    message: format!("Deployed {}.", status.version),
                    status,
                })
            })
        }
        ("GET", "/v1/monitor") => {
            return axum::Json(agent.monitor().await).into_response();
        }
        ("GET", "/v1/accounts/status") => {
            return axum::Json(agent.accounts_status().await).into_response();
        }
        ("POST", "/v1/accounts/restart") => match agent.accounts_restart().await {
            Ok(s) => return axum::Json(serde_json::json!({"ok": true, "message": "Account server restarted.", "accounts": s})).into_response(),
            Err(err) => Err(err),
        },
        ("POST", "/v1/accounts/deploy") => {
            let tag = serde_json::from_slice::<DeployBody>(&body).map(|b| b.tag).unwrap_or_default();
            match agent.accounts_deploy(&tag).await {
                Ok(s) => return axum::Json(serde_json::json!({"ok": true, "message": format!("Account server {} installed.", s.version), "accounts": s})).into_response(),
                Err(err) => Err(err),
            }
        }
        ("POST", "/v1/accounts/invite") => {
            let parsed = match serde_json::from_slice::<InviteBody>(&body) {
                Ok(b) => b,
                Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
            };
            match agent.accounts_invite(&parsed.key_sha256, &parsed.note).await {
                Ok(()) => return axum::Json(serde_json::json!({"ok": true, "message": "Invite key added."})).into_response(),
                Err(err) => Err(err),
            }
        }
        ("POST", "/v1/backups/restore") => {
            let name = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .unwrap_or_default();
            agent.restore_backup(&name).await.map(|status| {
                Reply::Action(AgentAction {
                    ok: true,
                    message: format!("Saved restored from backup {name}; server restarted."),
                    status,
                })
            })
        }
        ("POST", "/v1/agent/update") => {
            match agent.update_agent_now().await {
                Ok(message) => return axum::Json(serde_json::json!({"ok": true, "message": message})).into_response(),
                Err(err) => Err(crate::runtime::Fail { http: 502, message: err }),
            }
        }
        ("POST", "/v1/restart") => agent.restart().await.map(|status| {
            Reply::Action(AgentAction {
                ok: true,
                message: "Server restarted.".into(),
                status,
            })
        }),
        ("POST", "/v1/check") => match agent.report_available().await {
            Ok(status) => return axum::Json(status).into_response(),
            Err(err) => Err(crate::runtime::Fail { http: 502, message: err }),
        },
        ("POST", "/v1/rollback") => agent.rollback_previous().await.map(|status| {
            Reply::Action(AgentAction {
                ok: true,
                message: format!("Rolled back to {}.", status.version),
                status,
            })
        }),
        ("PATCH", "/v1/config") => {
            let patch = if body.is_empty() {
                serde_json::json!({})
            } else {
                match serde_json::from_slice::<serde_json::Value>(&body) {
                    Ok(value) => value,
                    Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
                }
            };
            match agent.patch_config(&patch) {
                Ok(value) => {
                    return axum::Json(serde_json::json!({
                        "ok": true,
                        "message": "Config updated.",
                        "config": public_config(&value),
                    }))
                    .into_response()
                }
                Err(err) => Err(err),
            }
        }
        ("GET", "/v1/diag/logs") => {
            let source = match crate::diag::parse_log_source(&query_string(&uri, "source")) {
                Ok(source) => source,
                Err(err) => return json_error(StatusCode::BAD_REQUEST, &err),
            };
            let failed = query_string(&uri, "name");
            let failed = if failed.is_empty() { None } else { Some(failed) };
            let lines = query_u64(&uri, "lines");
            let lines = if lines == 0 { 80 } else { lines as usize };
            match crate::diag::read_scoped_log(
                &agent.config.install_dir,
                &agent.config.work_dir,
                &agent.config.log_dir,
                source,
                failed.as_deref(),
                lines,
            ) {
                Ok(value) => return axum::Json(value).into_response(),
                Err(err) => Err(crate::runtime::Fail { http: 404, message: err }),
            }
        }
        ("GET", "/v1/diag/crashes") => {
            let hits = crate::diag::list_crashes(&agent.config.install_dir, &agent.config.work_dir);
            let crashes: Vec<_> = hits
                .into_iter()
                .map(|hit| {
                    serde_json::json!({
                        "id": hit.id,
                        "name": hit.name,
                        "bytes": hit.bytes,
                        "tooLarge": hit.too_large,
                    })
                })
                .collect();
            return axum::Json(serde_json::json!({ "crashes": crashes, "maxBytes": crate::diag::MAX_DUMP_BYTES })).into_response();
        }
        ("GET", "/v1/diag/crashes/download") => {
            let id = query_string(&uri, "id");
            let offset = query_u64(&uri, "offset");
            let limit = query_u64(&uri, "limit");
            let limit = if limit == 0 { crate::diag::MAX_CHUNK_BYTES } else { limit };
            let path = match crate::diag::crash_by_id(&agent.config.install_dir, &agent.config.work_dir, &id) {
                Ok(path) => path,
                Err(err) => return json_error(StatusCode::NOT_FOUND, &err),
            };
            match crate::diag::read_dump_chunk(&path, offset, limit) {
                Ok((bytes, total)) => {
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
                        .header("x-uxa-total", total.to_string())
                        .header("x-uxa-offset", offset.to_string())
                        .body(axum::body::Body::from(bytes))
                        .unwrap_or_else(|_| json_error(StatusCode::INTERNAL_SERVER_ERROR, "could not send the dump"))
                }
                Err(err) => return json_error(StatusCode::BAD_REQUEST, &err),
            }
        }
        ("GET", "/v1/diag/events") => {
            let roots = vec![
                agent.config.install_dir.clone(),
                std::path::PathBuf::from(r"C:\FNF"),
            ];
            return axum::Json(crate::diag::application_events(&agent.config.executable, &roots)).into_response();
        }
        ("GET", "/v1/diag/prereqs") => {
            return axum::Json(crate::prereq::prereq_report(
                &agent.config.install_dir,
                &agent.config.work_dir,
            ))
            .into_response();
        }
        ("POST", "/v1/wipe") => {
            let ok = serde_json::from_slice::<WipeBody>(&body)
                .map(|b| b.confirm == "WIPE")
                .unwrap_or(false);
            if !ok {
                return json_error(StatusCode::BAD_REQUEST, "Wipe needs {\"confirm\":\"WIPE\"}.");
            }
            agent.wipe().await.map(|status| {
                Reply::Action(AgentAction {
                    ok: true,
                    message: "Saves wiped (backup kept) and the server restarted.".into(),
                    status,
                })
            })
        }
        ("POST", "/v1/stop") => agent.stop().await.map(|status| {
            Reply::Action(AgentAction {
                ok: true,
                message: "Server stopped.".into(),
                status,
            })
        }),
        (
            "GET" | "POST" | "PATCH",
            "/v1/status" | "/v1/releases" | "/v1/logs" | "/v1/deploy" | "/v1/restart" | "/v1/stop" | "/v1/wipe" | "/v1/serverlog"
            | "/v1/monitor" | "/v1/accounts/status" | "/v1/accounts/restart" | "/v1/accounts/deploy" | "/v1/accounts/invite" | "/v1/agent/update" | "/v1/backups/restore"
            | "/v1/check" | "/v1/rollback" | "/v1/config" | "/v1/diag/logs" | "/v1/diag/crashes" | "/v1/diag/crashes/download" | "/v1/diag/events" | "/v1/diag/prereqs",
        ) => {
            return json_error(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed.");
        }
        _ => return json_error(StatusCode::NOT_FOUND, "Unknown agent route."),
    };

    match result {
        Ok(body) => body.into_response(),
        Err(err) => json_error(
            StatusCode::from_u16(err.http).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            &err.message,
        ),
    }
}

enum Reply {
    Status(launcher_core::AgentStatus),
    Releases(Vec<launcher_core::AgentReleaseSummary>),
    Logs(Vec<AgentLogLine>),
    Action(AgentAction),
}

impl IntoResponse for Reply {
    fn into_response(self) -> Response {
        match self {
            Self::Status(status) => Json(status).into_response(),
            Self::Releases(releases) => Json(releases).into_response(),
            Self::Logs(lines) => Json(serde_json::json!({ "lines": lines })).into_response(),
            Self::Action(action) => Json(action).into_response(),
        }
    }
}

fn router(agent: Arc<Agent>) -> Router {
    Router::new().fallback(dispatch).with_state(agent)
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    #[test]
    fn proxy_paths() {
        assert_eq!(
            account_proxy_target("/fnf-accounts/v1/signup"),
            Some("/v1/signup")
        );
        assert_eq!(
            account_proxy_target("/fnf-accounts:80/v1/signup"),
            Some("/v1/signup")
        );
        assert_eq!(
            account_proxy_target("/fnf-accounts:8788/v1/health"),
            Some("/v1/health")
        );
        assert_eq!(account_proxy_target("/fnf-accounts/v1/../v1/status"), None);
        assert_eq!(account_proxy_target("/fnf-accounts:/v1/signup"), None);
        assert_eq!(account_proxy_target("/fnf-accounts:x/v1/signup"), None);
        assert_eq!(account_proxy_target("/fnf-accountsX/v1/signup"), None);
        assert_eq!(account_proxy_target("/v1/signup"), None);
    }

    #[test]
    fn per_ip_signup_limit() {
        let ip = "203.0.113.250";
        for _ in 0..SIGNUPS_PER_IP_PER_MIN {
            assert!(rate_allow(ip, true));
        }
        assert!(!rate_allow(ip, true));
        assert!(rate_allow("203.0.113.251", true));
    }
}

pub async fn serve_listener(agent: Arc<Agent>, listener: TcpListener) -> Result<(), String> {
    agent.publish_health(true);
    let app = router(agent);
    axum::serve(listener, app)
        .await
        .map_err(|err| err.to_string())
}

pub async fn serve(agent: Arc<Agent>) -> Result<(), String> {
    agent.spawn_scheduler();
    agent.spawn_watchdog();
    if agent.config.tls_enabled() {
        let addr: SocketAddr = agent
            .config
            .listen
            .parse()
            .map_err(|err| format!("listen address: {err}"))?;
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            &agent.config.tls_cert,
            &agent.config.tls_key,
        )
        .await
        .map_err(|err| format!("TLS certificate: {err}"))?;
        agent.log(format!("listening on https://{addr}"));
        axum_server::bind_rustls(addr, tls)
            .serve(router(agent).into_make_service())
            .await
            .map_err(|err| err.to_string())
    } else {
        let listener = TcpListener::bind(&agent.config.listen)
            .await
            .map_err(|err| format!("could not listen on {}: {err}", agent.config.listen))?;
        let addr = listener.local_addr().map_err(|err| err.to_string())?;
        agent.log(format!(
            "listening on http://{addr} without TLS; every request still requires a signed admin key"
        ));
        serve_listener(agent, listener).await
    }
}
