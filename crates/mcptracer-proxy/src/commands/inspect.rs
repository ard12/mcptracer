use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Args;
use mcptracer_model::correlate;
use mcptracer_storage::Store;
use serde::Deserialize;
use serde_json::json;
use tokio::net::TcpListener;

const INDEX_HTML: &str = include_str!("../../assets/inspect/index.html");
const APP_JS: &str = include_str!("../../assets/inspect/app.js");
const APP_CSS: &str = include_str!("../../assets/inspect/app.css");

#[derive(Args)]
pub struct InspectArgs {
    /// Open directly to this session (id or unique prefix); omit to start on
    /// the session list.
    pub session_id: Option<String>,

    /// Local address to serve the read-only inspector UI on.
    #[arg(long, default_value = "127.0.0.1:4317")]
    pub listen: SocketAddr,

    /// Permit binding the inspector UI to a non-loopback address.
    #[arg(long)]
    pub allow_non_loopback: bool,
}

#[derive(Clone)]
struct InspectState {
    db_path: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ExportQuery {
    redact: Option<String>,
    allow_unredacted: bool,
    allow_sensitive_content: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct DiffQuery {
    explain_schema: bool,
}

/// Independent of the database state so every route, including static assets,
/// is protected before any handler can read recordings.
#[derive(Clone)]
struct InspectorSecurity {
    listen: SocketAddr,
    token: String,
}

impl InspectorSecurity {
    fn new(listen: SocketAddr) -> Self {
        Self {
            listen,
            token: uuid::Uuid::new_v4().simple().to_string(),
        }
    }

    fn permits_authority(&self, host: &str) -> bool {
        let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
            return false;
        };
        // `Authority` also accepts URI userinfo and nonnumeric ports; neither
        // is valid for a trusted HTTP Host header.
        if host.contains('@')
            || (authority.port_u16().is_none() && host != authority.host())
            || authority.port_u16().unwrap_or(80) != self.listen.port()
        {
            return false;
        }
        if authority.host().eq_ignore_ascii_case("localhost") {
            return self.listen.ip().is_loopback();
        }
        let Ok(ip) = authority
            .host()
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
        else {
            return false;
        };
        // Explicit non-loopback/wildcard binds support literal IP URLs only;
        // an arbitrary DNS name must never become trusted through rebinding.
        ip == self.listen.ip()
            || (self.listen.ip().is_unspecified()
                && !ip.is_unspecified()
                && ip.is_ipv4() == self.listen.ip().is_ipv4())
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

async fn protect_inspector(
    State(security): State<InspectorSecurity>,
    request: Request,
    next: Next,
) -> Response {
    let headers = request.headers();
    let host = single_header(headers, "host");
    let origin_allowed = !headers.contains_key(header::ORIGIN)
        || single_header(headers, "origin").is_some_and(|origin| {
            host.is_some_and(|host| origin.eq_ignore_ascii_case(&format!("http://{host}")))
        });
    let is_api = request.uri().path() == "/api" || request.uri().path().starts_with("/api/");
    let mut response = if !host.is_some_and(|host| security.permits_authority(host))
        || !origin_allowed
    {
        (
            StatusCode::FORBIDDEN,
            "Inspector Host or Origin is not allowed",
        )
            .into_response()
    } else if is_api
        && single_header(headers, "authorization").and_then(|value| value.strip_prefix("Bearer "))
            != Some(security.token.as_str())
    {
        let mut response = (
            StatusCode::UNAUTHORIZED,
            "Open the inspector link printed by mcptracer",
        )
            .into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, "Bearer".parse().unwrap());
        response
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("content-security-policy", "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'".parse().unwrap());
    response
}

fn inspector_router(db_path: PathBuf, security: InspectorSecurity) -> Router {
    Router::new()
        .route("/", get(index_page))
        .route("/session/{id}", get(index_page))
        .route("/diff/{id1}/{id2}", get(index_page))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/{id}", get(api_session_detail))
        .route("/api/sessions/{id}/export", get(api_session_export))
        .route("/api/diff/{id1}/{id2}", get(api_diff))
        .with_state(InspectState { db_path })
        .layer(middleware::from_fn_with_state(security, protect_inspector))
}

pub async fn run(args: InspectArgs, db_path: PathBuf) -> Result<()> {
    if !args.listen.ip().is_loopback() && !args.allow_non_loopback {
        return Err(anyhow!(
            "refusing to bind the inspector UI to a non-loopback address without --allow-non-loopback"
        ));
    }
    // Fail fast on a bad --db path instead of on the first browser request.
    Store::open(&db_path).context("failed to open session database")?;
    if let Some(requested) = &args.session_id {
        Store::open(&db_path)?
            .get_session_summary(requested)
            .with_context(|| format!("unknown session: {requested}"))?;
    }

    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("failed to bind inspector UI listener at {}", args.listen))?;
    let local_addr = listener.local_addr()?;
    let security = InspectorSecurity::new(local_addr);
    let mut browser_addr = local_addr;
    if browser_addr.ip().is_unspecified() {
        browser_addr.set_ip(if browser_addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    let start_path = match &args.session_id {
        Some(id) => format!("/session/{id}"),
        None => "/".to_string(),
    };
    eprintln!(
        "[mcptracer] inspector UI (read-only) at http://{browser_addr}{start_path}#token={}",
        security.token
    );
    eprintln!("[mcptracer] Ctrl-C to stop");

    axum::serve(listener, inspector_router(db_path, security))
        .with_graceful_shutdown(wait_for_shutdown())
        .await
        .context("inspector UI server failed")?;
    Ok(())
}

async fn wait_for_shutdown() {
    // A failed handler registration must not stop the server by itself; see
    // `crate::shutdown::next_stop_request`. SIGTERM is honored too, so a
    // process manager's stop request shuts down gracefully.
    let mut stop = crate::shutdown::listen_for_stop_requests();
    crate::shutdown::next_stop_request(&mut stop).await;
}

async fn index_page() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/javascript")], APP_JS)
}

async fn app_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css")], APP_CSS)
}

async fn api_sessions(State(state): State<InspectState>) -> Response {
    let store = match Store::open(&state.db_path) {
        Ok(store) => store,
        Err(error) => return error_response(&error),
    };
    match store.list_sessions(200) {
        Ok(sessions) => {
            let payload: Vec<_> = sessions
                .iter()
                .map(|s| {
                    json!({
                        "id": s.id,
                        "client": s.client,
                        "server_command": s.server_command,
                        "transport": s.transport,
                        "started_at_ns": s.started_at_ns,
                        "ended_at_ns": s.ended_at_ns,
                        "total_messages": s.total_messages,
                        "dropped_messages": s.dropped_messages,
                        "redaction_policy": s.redaction_policy,
                    })
                })
                .collect();
            Json(payload).into_response()
        }
        Err(error) => error_response(&error),
    }
}

async fn api_session_detail(State(state): State<InspectState>, Path(id): Path<String>) -> Response {
    let store = match Store::open(&state.db_path) {
        Ok(store) => store,
        Err(error) => return error_response(&error),
    };
    let summary = match store.get_session_summary(&id) {
        Ok(summary) => summary,
        Err(error) => return (StatusCode::NOT_FOUND, error.to_string()).into_response(),
    };
    let messages = match store.get_messages(&summary.id) {
        Ok(messages) => messages,
        Err(error) => return error_response(&error),
    };
    let model = correlate(&messages);
    let health = crate::session_health::assess_capture(&summary, &messages);

    let messages_json: Vec<_> = messages
        .iter()
        .map(|msg| {
            let payload: serde_json::Value = serde_json::from_str(&msg.payload)
                .unwrap_or_else(|_| serde_json::Value::String(msg.payload.clone()));
            json!({
                "seq": msg.seq,
                "ts_ns": msg.ts_ns,
                "direction": msg.direction,
                "message_kind": msg.message_kind,
                "rpc_id": msg.rpc_id,
                "method": msg.method,
                "tool_name": msg.tool_name,
                "is_error": msg.is_error,
                "error_code": msg.error_code,
                "payload": payload,
            })
        })
        .collect();

    Json(json!({
        "session": {
            "id": summary.id,
            "client": summary.client,
            "server_command": summary.server_command,
            "transport": summary.transport,
            "started_at_ns": summary.started_at_ns,
            "ended_at_ns": summary.ended_at_ns,
            "total_messages": summary.total_messages,
            "dropped_messages": summary.dropped_messages,
            "redaction_policy": summary.redaction_policy,
        },
        "messages": messages_json,
        "model": model,
        "capture_health": {
            "healthy": health.is_healthy(),
            "issues": health.issues,
        },
    }))
    .into_response()
}

async fn api_session_export(
    State(state): State<InspectState>,
    Path(id): Path<String>,
    Query(options): Query<ExportQuery>,
) -> Response {
    let store = match Store::open(&state.db_path) {
        Ok(store) => store,
        Err(error) => return error_response(&error),
    };
    let summary = match store.get_session_summary(&id) {
        Ok(summary) => summary,
        Err(error) => return (StatusCode::NOT_FOUND, error.to_string()).into_response(),
    };
    let (_document, bytes) = match super::export::build_validated_artifact(
        &store,
        &summary.id,
        options.redact.as_deref(),
        options.allow_unredacted,
        options.allow_sensitive_content,
    ) {
        Ok(artifact) => artifact,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };

    let filename = format!("{}.mtrace", summary.id);
    let disposition = match format!("attachment; filename=\"{filename}\"").parse() {
        Ok(val) => val,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "invalid header value").into_response()
        }
    };
    let mut response = (StatusCode::OK, bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    headers.insert(header::CONTENT_DISPOSITION, disposition);
    response
}

async fn api_diff(
    State(state): State<InspectState>,
    Path((id1, id2)): Path<(String, String)>,
    Query(options): Query<DiffQuery>,
) -> Response {
    let store = match Store::open(&state.db_path) {
        Ok(store) => store,
        Err(error) => return error_response(&error),
    };
    let summary1 = match store.get_session_summary(&id1) {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::NOT_FOUND, format!("session A not found: {e}")).into_response()
        }
    };
    let summary2 = match store.get_session_summary(&id2) {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::NOT_FOUND, format!("session B not found: {e}")).into_response()
        }
    };
    if let Err(error) =
        crate::session_health::require_healthy_session(&store, &summary1.id, "inspector diff")
    {
        return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
    }
    if let Err(error) =
        crate::session_health::require_healthy_session(&store, &summary2.id, "inspector diff")
    {
        return (StatusCode::BAD_REQUEST, error.to_string()).into_response();
    }
    let msgs1 = match store.get_messages(&summary1.id) {
        Ok(m) => m,
        Err(e) => return error_response(&e),
    };
    let msgs2 = match store.get_messages(&summary2.id) {
        Ok(m) => m,
        Err(e) => return error_response(&e),
    };

    let report = mcptracer_model::diff::diff_sessions(
        &msgs1,
        &msgs2,
        &mcptracer_model::diff::DiffOptions::default(),
    );

    let mut response = json!({
        "session_a": {
            "id": summary1.id,
            "client": summary1.client,
            "server_command": summary1.server_command,
            "transport": summary1.transport,
            "started_at_ns": summary1.started_at_ns,
            "ended_at_ns": summary1.ended_at_ns,
            "total_messages": summary1.total_messages,
        },
        "session_b": {
            "id": summary2.id,
            "client": summary2.client,
            "server_command": summary2.server_command,
            "transport": summary2.transport,
            "started_at_ns": summary2.started_at_ns,
            "ended_at_ns": summary2.ended_at_ns,
            "total_messages": summary2.total_messages,
        },
        "report": report,
    });
    if options.explain_schema {
        let analysis = mcptracer_model::diff::explain_tool_schema_changes(&msgs1, &msgs2);
        response["schema_explanation_version"] = json!(1);
        response["baseline_catalog_complete"] = json!(analysis.baseline_catalog_complete);
        response["candidate_catalog_complete"] = json!(analysis.candidate_catalog_complete);
        response["analysis_status"] = json!(analysis.analysis_status);
        response["schema_explanations"] = json!(analysis.schema_explanations);
    }
    Json(response).into_response()
}

fn error_response(error: &anyhow::Error) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use mcptracer_protocol::{Direction, McpMessage};
    use serde_json::json;

    /// Structurally unique per call, not just probabilistically so: a
    /// timestamp alone collides when two tests enter this function within
    /// the same clock tick (observed on Windows, whose `SystemTime`
    /// granularity is ~15.6ms), causing them to share one SQLite file. The
    /// process id plus a monotonically increasing counter guarantees a fresh
    /// path on every call regardless of clock resolution or thread scheduling.
    fn temp_db_path(name: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "mcptracer-inspect-{}-{unique}-{name}",
            std::process::id()
        ))
    }

    fn seed_store(path: &std::path::Path) -> String {
        let store = Store::open(path).unwrap();
        let session_id = store
            .create_session("inspector-test", "cmd", "stdio", 0)
            .unwrap();
        store
            .write_message(
                &session_id,
                &McpMessage {
                    seq: 0,
                    timestamp_ns: 0,
                    direction: Direction::ClientToServer,
                    payload: json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call"}),
                    payload_bytes: 10,
                },
            )
            .unwrap();
        store
            .write_message(
                &session_id,
                &McpMessage {
                    seq: 1,
                    timestamp_ns: 1,
                    direction: Direction::ServerToClient,
                    payload: json!({"jsonrpc": "2.0", "id": 1, "result": {"content": []}}),
                    payload_bytes: 10,
                },
            )
            .unwrap();
        store.close_session(&session_id, 100).unwrap();
        session_id
    }

    #[tokio::test]
    async fn api_sessions_lists_recorded_sessions() {
        let db_path = temp_db_path("sessions.db");
        let session_id = seed_store(&db_path);

        let state = InspectState { db_path };
        let response = api_sessions(State(state)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 1);
        assert_eq!(value[0]["id"], session_id);
    }

    #[tokio::test]
    async fn api_session_detail_includes_messages_and_correlated_model() {
        let db_path = temp_db_path("sessions.db");
        let session_id = seed_store(&db_path);

        let state = InspectState { db_path };
        let response = api_session_detail(State(state), Path(session_id.clone())).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["session"]["id"], session_id);
        assert_eq!(value["messages"].as_array().unwrap().len(), 2);
        assert_eq!(value["model"]["stats"]["total_exchanges"], 1);
        assert_eq!(value["capture_health"]["healthy"], true);
        assert!(value["capture_health"]["issues"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn api_session_detail_reports_capture_loss_with_shared_health_predicate() {
        let db_path = temp_db_path("detail-loss.db");
        let session_id = seed_store(&db_path);
        let store = Store::open(&db_path).unwrap();
        store.increment_dropped_messages(&session_id, 1).unwrap();
        let expected = crate::session_health::inspect_session(&store, &session_id).unwrap();
        let response = api_session_detail(State(InspectState { db_path }), Path(session_id)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["capture_health"]["healthy"], false);
        assert_eq!(
            value["capture_health"]["issues"],
            serde_json::to_value(expected.report.issues).unwrap()
        );
    }

    #[tokio::test]
    async fn api_session_detail_404s_for_unknown_session() {
        let db_path = temp_db_path("sessions.db");
        Store::open(&db_path).unwrap();

        let state = InspectState { db_path };
        let response = api_session_detail(State(state), Path("nonexistent".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn api_session_export_returns_mtrace_bytes_and_attachment_header() {
        let db_path = temp_db_path("export.db");
        let session_id = seed_store(&db_path);

        let state = InspectState { db_path };
        let response = api_session_export(
            State(state),
            Path(session_id.clone()),
            Query(ExportQuery {
                allow_unredacted: true,
                ..ExportQuery::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        let content_disp = response.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap();
        assert!(content_disp.contains(&format!("{session_id}.mtrace")));

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let decoded = mcptracer_storage::mtrace::decode(&body, true).unwrap();
        assert_eq!(decoded.session.client, "inspector-test");
    }

    #[tokio::test]
    async fn api_session_export_requires_unredacted_consent_and_sensitive_content_consent() {
        let db_path = temp_db_path("export-consent.db");
        let session_id = seed_store(&db_path);
        let state = InspectState { db_path };
        let denied = api_session_export(
            State(state.clone()),
            Path(session_id.clone()),
            Query(ExportQuery::default()),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::BAD_REQUEST);
        let denied_body = to_bytes(denied.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&denied_body).contains("--allow-unredacted"));

        let store = Store::open(&state.db_path).unwrap();
        let sensitive_session = store
            .create_session("inspector-test", "cmd", "stdio", 0)
            .unwrap();
        store
            .write_message(
                &sensitive_session,
                &McpMessage {
                    seq: 0,
                    timestamp_ns: 0,
                    direction: Direction::ClientToServer,
                    payload: json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "tools/call",
                        "params": {
                            "name": "echo",
                            "arguments": {"url": "https://example.test/?token=synthetic-secret"}
                        }
                    }),
                    payload_bytes: 64,
                },
            )
            .unwrap();
        store.close_session(&sensitive_session, 1).unwrap();
        let response = api_session_export(
            State(state.clone()),
            Path(sensitive_session.clone()),
            Query(ExportQuery {
                allow_unredacted: true,
                ..ExportQuery::default()
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("sensitive-content finding"));
        assert!(!text.contains("synthetic-secret"));

        let override_response = api_session_export(
            State(state),
            Path(sensitive_session),
            Query(ExportQuery {
                allow_unredacted: true,
                allow_sensitive_content: true,
                ..ExportQuery::default()
            }),
        )
        .await;
        assert_eq!(override_response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_diff_compares_two_sessions_and_returns_diff_report() {
        let db_path = temp_db_path("diff.db");
        let session_a = seed_store(&db_path);

        // Seed a second session
        let store = Store::open(&db_path).unwrap();
        let session_b = store
            .create_session("inspector-test-2", "cmd2", "stdio", 0)
            .unwrap();
        store
            .write_message(
                &session_b,
                &McpMessage {
                    seq: 0,
                    timestamp_ns: 0,
                    direction: Direction::ClientToServer,
                    payload: json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "other_tool"}}),
                    payload_bytes: 20,
                },
            )
            .unwrap();
        store
            .write_message(
                &session_b,
                &McpMessage {
                    seq: 1,
                    timestamp_ns: 1,
                    direction: Direction::ServerToClient,
                    payload: json!({"jsonrpc": "2.0", "id": 1, "result": {"content": []}}),
                    payload_bytes: 20,
                },
            )
            .unwrap();
        store.close_session(&session_b, 100).unwrap();

        let state = InspectState { db_path };
        let response = api_diff(
            State(state.clone()),
            Path((session_a.clone(), session_b.clone())),
            Query(DiffQuery::default()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["session_a"]["id"], session_a);
        assert_eq!(value["session_b"]["id"], session_b);
        assert!(value["report"]["changed"].is_array());
        assert!(value.get("schema_explanation_version").is_none());

        let explained = api_diff(
            State(InspectState {
                db_path: state.db_path.clone(),
            }),
            Path((session_a.clone(), session_b.clone())),
            Query(DiffQuery {
                explain_schema: true,
            }),
        )
        .await;
        assert_eq!(explained.status(), StatusCode::OK);
        let body = to_bytes(explained.into_body(), usize::MAX).await.unwrap();
        let explained: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(explained["schema_explanation_version"], 1);
        assert_eq!(explained["analysis_status"], "incomplete_catalog");
        assert!(explained["schema_explanations"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[tokio::test]
    async fn api_diff_refuses_an_unclosed_session() {
        let db_path = temp_db_path("diff-health.db");
        let session_a = seed_store(&db_path);
        let store = Store::open(&db_path).unwrap();
        let session_b = store
            .create_session("incomplete", "cmd", "stdio", 100)
            .unwrap();
        store
            .write_message(
                &session_b,
                &McpMessage {
                    seq: 0,
                    timestamp_ns: 100,
                    direction: Direction::ClientToServer,
                    payload: json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
                    payload_bytes: 32,
                },
            )
            .unwrap();
        drop(store);

        let response = api_diff(
            State(InspectState { db_path }),
            Path((session_a, session_b)),
            Query(DiffQuery::default()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("is unhealthy and cannot be used"));
    }

    #[test]
    fn authority_validation_covers_loopback_ipv6_wildcards_and_ports() {
        for (listen, allowed, rejected) in [
            (
                "127.0.0.1:4317",
                vec!["127.0.0.1:4317", "localhost:4317"],
                vec![
                    "evil.example:4317",
                    "127.0.0.1.evil.example:4317",
                    "127.0.0.1:4318",
                    "127.0.0.2:4317",
                    "user@127.0.0.1:4317",
                ],
            ),
            (
                "[::1]:4317",
                vec!["[::1]:4317", "localhost:4317"],
                vec!["[::1]:4318", "[::2]:4317", "evil.example:4317"],
            ),
            (
                "0.0.0.0:4317",
                vec!["127.0.0.1:4317", "192.0.2.1:4317"],
                vec!["evil.example:4317", "192.0.2.1:4318", "[::1]:4317"],
            ),
            (
                "127.0.0.1:80",
                vec!["127.0.0.1", "localhost"],
                vec!["127.0.0.1:invalid", "localhost:443"],
            ),
        ] {
            let security = InspectorSecurity::new(listen.parse().unwrap());
            for host in allowed {
                assert!(security.permits_authority(host), "{listen}: {host}");
            }
            for host in rejected {
                assert!(!security.permits_authority(host), "{listen}: {host}");
            }
        }
    }

    #[tokio::test]
    async fn router_guards_real_requests_before_disclosing_sessions() {
        let db_path = temp_db_path("guarded.db");
        let session_id = seed_store(&db_path);
        let store = Store::open(&db_path).unwrap();
        let incomplete_id = store
            .create_session("incomplete", "synthetic-server", "stdio", 200)
            .unwrap();
        let sensitive_id = store
            .create_session("sensitive", "synthetic-server", "stdio", 300)
            .unwrap();
        store
            .write_message(
                &sensitive_id,
                &McpMessage {
                    seq: 0,
                    timestamp_ns: 300,
                    direction: Direction::ClientToServer,
                    payload: json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "tools/call",
                        "params": {"name":"echo","arguments":{"url":"https://example.test/?token=http-secret-fixture"}}
                    }),
                    payload_bytes: 64,
                },
            )
            .unwrap();
        store.close_session(&sensitive_id, 301).unwrap();
        drop(store);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let security = InspectorSecurity::new(address);
        let token = security.token.clone();
        assert_ne!(token, InspectorSecurity::new(address).token);
        let app = inspector_router(db_path, security);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let base = format!("http://{address}");
        for path in [
            "/api/sessions",
            &format!("/api/sessions/{session_id}"),
            "/api/sessions/missing",
        ] {
            for candidate in [None, Some("wrong-token")] {
                let mut request = client.get(format!("{base}{path}"));
                if let Some(candidate) = candidate {
                    request = request.bearer_auth(candidate);
                }
                let response = request.send().await.unwrap();
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                assert!(!response.text().await.unwrap().contains(&session_id));
            }
        }
        let response = client
            .get(format!("{base}/api/sessions?token={token}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        for (host, origin) in [
            ("evil.example:4317".to_string(), None),
            (
                format!("127.0.0.1:{}", address.port().wrapping_add(1)),
                None,
            ),
            (address.to_string(), Some("http://evil.example".to_string())),
            (address.to_string(), Some("null".to_string())),
            (address.to_string(), Some(format!("https://{address}"))),
        ] {
            for path in ["/", "/app.js", "/api/sessions"] {
                let mut request = client
                    .get(format!("{base}{path}"))
                    .header(header::HOST, &host)
                    .bearer_auth(&token);
                if let Some(origin) = &origin {
                    request = request.header(header::ORIGIN, origin);
                }
                let response = request.send().await.unwrap();
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
                assert!(!response.text().await.unwrap().contains(&session_id));
            }
        }
        let mut headers = HeaderMap::new();
        headers.append(header::ORIGIN, base.parse().unwrap());
        headers.append(header::ORIGIN, "http://evil.example".parse().unwrap());
        let response = client
            .get(format!("{base}/api/sessions"))
            .headers(headers)
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let mut headers = HeaderMap::new();
        headers.append(
            header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers.append(header::AUTHORIZATION, "Bearer wrong-token".parse().unwrap());
        let response = client
            .get(format!("{base}/api/sessions"))
            .headers(headers)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        for origin in [None, Some(&base)] {
            let mut request = client
                .get(format!("{base}/api/sessions/{session_id}"))
                .bearer_auth(&token);
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()["referrer-policy"], "no-referrer");
            assert_eq!(response.headers()["x-content-type-options"], "nosniff");
            assert!(response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'"));
            let value: serde_json::Value = response.json().await.unwrap();
            assert_eq!(value["session"]["id"], session_id);
        }
        let response = client
            .get(format!("{base}/api/sessions/{session_id}/export"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let diagnostic = response.text().await.unwrap();
        assert!(diagnostic.contains("--allow-unredacted"), "{diagnostic}");

        let response = client
            .get(format!(
                "{base}/api/sessions/{session_id}/export?allow_unredacted=true"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let artifact = response.bytes().await.unwrap();
        assert_eq!(
            mcptracer_storage::mtrace::decode(&artifact, true)
                .unwrap()
                .session
                .client,
            "inspector-test"
        );

        let response = client
            .get(format!(
                "{base}/api/sessions/{sensitive_id}/export?allow_unredacted=true"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let diagnostic = response.text().await.unwrap();
        assert!(diagnostic.contains("sensitive-content finding"));
        assert!(!diagnostic.contains("http-secret-fixture"));
        let response = client
            .get(format!("{base}/api/sessions/{sensitive_id}/export?allow_unredacted=true&allow_sensitive_content=true"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = client
            .get(format!("{base}/api/diff/{session_id}/{incomplete_id}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(response
            .text()
            .await
            .unwrap()
            .contains("is unhealthy and cannot be used"));

        let response = client
            .get(format!("{base}/api/diff/{session_id}/{session_id}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let default_diff: serde_json::Value = response.json().await.unwrap();
        assert!(default_diff.get("schema_explanation_version").is_none());

        let response = client
            .get(format!(
                "{base}/api/diff/{session_id}/{session_id}?explain_schema=true"
            ))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let explained_diff: serde_json::Value = response.json().await.unwrap();
        assert_eq!(explained_diff["schema_explanation_version"], 1);
        assert_eq!(explained_diff["analysis_status"], "incomplete_catalog");
        assert!(explained_diff["schema_explanations"]
            .as_array()
            .unwrap()
            .is_empty());

        for path in [
            "/",
            "/app.js",
            "/app.css",
            &format!("/session/{session_id}"),
        ] {
            let response = client.get(format!("{base}{path}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let text = response.text().await.unwrap();
            assert!(!text.contains(&token));
            assert!(!text.contains("inspector-test"));
        }
        server.abort();
    }
}
