use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use clap::Args;
use mcptracer_model::correlate;
use mcptracer_storage::Store;
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
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/api/sessions", get(api_sessions))
        .route("/api/sessions/{id}", get(api_session_detail))
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
    }))
    .into_response()
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
        assert_eq!(value["messages"].as_array().unwrap().len(), 1);
        assert_eq!(value["model"]["stats"]["total_exchanges"], 1);
    }

    #[tokio::test]
    async fn api_session_detail_404s_for_unknown_session() {
        let db_path = temp_db_path("sessions.db");
        Store::open(&db_path).unwrap();

        let state = InspectState { db_path };
        let response = api_session_detail(State(state), Path("nonexistent".to_string())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
