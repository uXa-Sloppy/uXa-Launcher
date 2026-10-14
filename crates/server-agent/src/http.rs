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

async fn dispatch(
    State(agent): State<Arc<Agent>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path();
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

    let result = match (method.as_str(), path) {
        ("GET", "/v1/status") => Ok(Reply::Status(agent.status().await)),
        ("GET", "/v1/releases") => agent.releases().await.map(Reply::Releases),
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
        ("POST", "/v1/restart") => agent.restart().await.map(|status| {
            Reply::Action(AgentAction {
                ok: true,
                message: "Server restarted.".into(),
                status,
            })
        }),
        ("POST", "/v1/stop") => agent.stop().await.map(|status| {
            Reply::Action(AgentAction {
                ok: true,
                message: "Server stopped.".into(),
                status,
            })
        }),
        (
            "GET" | "POST",
            "/v1/status" | "/v1/releases" | "/v1/logs" | "/v1/deploy" | "/v1/restart" | "/v1/stop",
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

pub async fn serve_listener(agent: Arc<Agent>, listener: TcpListener) -> Result<(), String> {
    let app = router(agent);
    axum::serve(listener, app)
        .await
        .map_err(|err| err.to_string())
}

pub async fn serve(agent: Arc<Agent>) -> Result<(), String> {
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
        let addr = listener
            .local_addr()
            .map_err(|err| err.to_string())?;
        agent.log(format!(
            "listening on http://{addr} without TLS; every request still requires a signed admin key"
        ));
        serve_listener(agent, listener).await
    }
}
