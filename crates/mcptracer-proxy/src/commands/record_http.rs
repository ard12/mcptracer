use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{CONNECTION, CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, HeaderName, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use clap::Args;
use futures_util::{stream, StreamExt};
use mcptracer_protocol::{Direction, StreamableHttpTransport, Transport};
use mcptracer_redact::{RedactionPolicy, Redactor};
use mcptracer_storage::Store;
use reqwest::redirect::Policy as RedirectPolicy;
use tokio::net::TcpListener;
use tokio::sync::{mpsc as tokio_mpsc, Mutex};
use tokio::task::JoinSet;
use tracing::warn;
use url::Url;

use crate::session_writer::{
    finish_storage_writer, now_ns, spawn_storage_writer, try_record, StorageEvent,
    StorageQueueBudget, MAX_FRAME_BYTES, STORAGE_QUEUE_MAX_BYTES,
};
use crate::sse::{SseDecoder, SseEvent};

const RESPONSE_CAPTURE_CHANNEL_CAPACITY: usize = 64;
const REQUEST_FORWARD_CHANNEL_CAPACITY: usize = 16;

#[derive(Args)]
pub struct RecordHttpArgs {
    /// Local address that accepts MCP Streamable HTTP requests.
    #[arg(long, default_value = "127.0.0.1:8787")]
    pub listen: SocketAddr,

    /// Absolute upstream Streamable HTTP endpoint URL.
    #[arg(long)]
    pub target: String,

    #[arg(long, default_value = "unknown")]
    pub client: String,

    /// Redaction policy applied to stored payloads: `none` or `default`.
    #[arg(long, default_value = "none")]
    pub redact: String,

    /// Extra JSON field names to mask, comma-separated. Requires
    /// `--redact default` and is stored as the `custom` policy.
    #[arg(long, value_delimiter = ',')]
    pub redact_keys: Vec<String>,

    /// Permit binding the local reverse proxy to a non-loopback address.
    #[arg(long)]
    pub allow_non_loopback: bool,

    /// Group otherwise-stateless requests carrying the same value in this
    /// header into one MCPTracer recording. Useful for MCP 2026-07-28, which
    /// has no protocol session id. The value is held only in process memory
    /// for correlation and is never persisted or printed.
    #[arg(long, value_name = "HEADER")]
    pub group_stateless_by_header: Option<HeaderName>,
}

/// Identifies which recorded session a given HTTP exchange belongs to.
///
/// The MCP Streamable HTTP transport only has a shared identity once the
/// server assigns `Mcp-Session-Id` in a response; before that (or for a
/// stateless server that never assigns one) there is no protocol-level way
/// to correlate one request with any other. Rather than guess, every
/// header-less request gets its own one-shot `Provisional` session: this is
/// semantically honest (it never merges exchanges that share no verified
/// identity) and keeps key resolution synchronous and request-header-driven,
/// which is what lets the streaming forward/capture pipeline below resolve a
/// session before it has to start moving bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LogicalSessionKey {
    Established(String),
    StatelessGroup(String),
    Provisional(u64),
}

fn describe_key(key: &LogicalSessionKey) -> String {
    match key {
        LogicalSessionKey::Established(id) => format!("session {id}"),
        LogicalSessionKey::StatelessGroup(_) => {
            "stateless correlation group (value not logged)".to_string()
        }
        LogicalSessionKey::Provisional(n) => {
            format!("provisional exchange #{n} (no Mcp-Session-Id issued)")
        }
    }
}

/// Per-logical-session recording handles. Cheap to clone: every field is
/// itself a channel handle or shared counter, so request/response capture
/// tasks can each hold their own copy without touching the session registry.
#[derive(Clone)]
struct HttpSession {
    session_id: String,
    storage_tx: SyncSender<StorageEvent>,
    seq: Arc<AtomicU64>,
    dropped_messages: Arc<AtomicU64>,
    queue_budget: Arc<StorageQueueBudget>,
}

/// Owns the one thing a session's `HttpSession` handle can't hold: the
/// storage writer thread. Kept out of `HttpSession` (and thus never cloned)
/// because a `JoinHandle` can only be joined once, when the session closes.
struct SessionEntry {
    session: HttpSession,
    handle: thread::JoinHandle<()>,
}

#[derive(Clone)]
struct HttpState {
    target: Url,
    client: reqwest::Client,
    db_path: std::path::PathBuf,
    client_name: String,
    policy: RedactionPolicy,
    sessions: Arc<Mutex<HashMap<LogicalSessionKey, SessionEntry>>>,
    provisional_counter: Arc<AtomicU64>,
    stateless_group_header: Option<HeaderName>,
    capture_tasks: Arc<Mutex<JoinSet<()>>>,
}

#[derive(Clone, Copy)]
enum ResponseCaptureKind {
    Json,
    Sse,
}

pub async fn run(args: RecordHttpArgs, db_path: std::path::PathBuf) -> Result<()> {
    if !args.listen.ip().is_loopback() && !args.allow_non_loopback {
        return Err(anyhow!(
            "refusing to bind record-http to a non-loopback address without --allow-non-loopback"
        ));
    }

    let target = parse_target(&args.target)?;
    let policy = RedactionPolicy::from_cli(&args.redact, &args.redact_keys)
        .map_err(|error| anyhow!(error))?;

    let client = reqwest::Client::builder()
        .redirect(RedirectPolicy::none())
        .build()
        .context("failed to create Streamable HTTP client")?;
    let state = HttpState {
        target,
        client,
        db_path,
        client_name: args.client.clone(),
        policy,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        provisional_counter: Arc::new(AtomicU64::new(0)),
        stateless_group_header: args.group_stateless_by_header,
        capture_tasks: Arc::new(Mutex::new(JoinSet::new())),
    };

    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("failed to bind Streamable HTTP listener at {}", args.listen))?;
    let local_addr = listener.local_addr()?;
    eprintln!(
        "[mcptracer] recording Streamable HTTP traffic at http://{} -> {} (one recorded session per logical Mcp-Session-Id)",
        local_addr,
        sanitized_target(&state.target)
    );

    let app = Router::new()
        .fallback(any(proxy_request))
        .with_state(state.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(wait_for_shutdown())
        .await
        .context("Streamable HTTP proxy failed")?;

    wait_for_capture_tasks(&state).await;
    close_all_sessions(&state).await;
    Ok(())
}

async fn wait_for_shutdown() {
    // A failed handler registration must not stop the server by itself; see
    // `crate::shutdown::next_stop_request`. SIGTERM is honored too, so a
    // process manager's stop request shuts down gracefully.
    let mut stop = crate::shutdown::listen_for_stop_requests();
    crate::shutdown::next_stop_request(&mut stop).await;
}

async fn wait_for_capture_tasks(state: &HttpState) {
    let mut tasks = state.capture_tasks.lock().await;
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            warn!("HTTP capture task failed: {error}");
        }
    }
}

/// Resolve which logical session an incoming request belongs to, based only
/// on its own `Mcp-Session-Id` request header or an explicitly configured
/// stateless correlation header. Response headers are never consulted for
/// routing: see the `LogicalSessionKey` doc comment for why.
fn session_key_for_request(state: &HttpState, headers: &HeaderMap) -> LogicalSessionKey {
    if let Some(id) = headers
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        return LogicalSessionKey::Established(id.to_string());
    }
    if let Some(group) = state
        .stateless_group_header
        .as_ref()
        .and_then(|name| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        return LogicalSessionKey::StatelessGroup(group.to_string());
    }
    LogicalSessionKey::Provisional(state.provisional_counter.fetch_add(1, Ordering::Relaxed))
}

fn create_http_session(state: &HttpState) -> Result<(HttpSession, thread::JoinHandle<()>)> {
    let store = Store::open(&state.db_path)?;
    let session_id = store.create_session(
        &state.client_name,
        &sanitized_target(&state.target),
        StreamableHttpTransport.name(),
        now_ns(),
    )?;
    let redactor = Redactor::new(state.policy.clone());
    if redactor.is_active() {
        store.set_redaction_policy(
            &session_id,
            state.policy.as_str(),
            state.policy.custom_keys(),
        )?;
    }

    let (storage_tx, storage_rx) = mpsc::sync_channel::<StorageEvent>(4096);
    let queue_budget = Arc::new(StorageQueueBudget::new(STORAGE_QUEUE_MAX_BYTES));
    let handle = spawn_storage_writer(
        store,
        session_id.clone(),
        redactor,
        storage_rx,
        Arc::clone(&queue_budget),
    );
    let session = HttpSession {
        session_id,
        storage_tx,
        seq: Arc::new(AtomicU64::new(0)),
        dropped_messages: Arc::new(AtomicU64::new(0)),
        queue_budget,
    };
    Ok((session, handle))
}

async fn resolve_session(state: &HttpState, key: LogicalSessionKey) -> Result<HttpSession> {
    let mut sessions = state.sessions.lock().await;
    if let Some(entry) = sessions.get(&key) {
        return Ok(entry.session.clone());
    }
    let (session, handle) = create_http_session(state)?;
    eprintln!(
        "[mcptracer] recording {} as {}",
        describe_key(&key),
        session.session_id
    );
    sessions.insert(
        key,
        SessionEntry {
            session: session.clone(),
            handle,
        },
    );
    Ok(session)
}

async fn finalize_session_entry(
    session_id: String,
    storage_tx: SyncSender<StorageEvent>,
    handle: thread::JoinHandle<()>,
    dropped_messages: u64,
) {
    if let Err(error) = finish_storage_writer(storage_tx, handle, now_ns(), dropped_messages) {
        warn!("failed to finalize HTTP session {session_id}: {error}");
    } else {
        eprintln!("[mcptracer] session {session_id} ended");
    }
}

async fn close_session(state: &HttpState, key: &LogicalSessionKey) {
    let entry = state.sessions.lock().await.remove(key);
    let Some(SessionEntry { session, handle }) = entry else {
        return;
    };
    let dropped = session.dropped_messages.load(Ordering::Relaxed);
    finalize_session_entry(session.session_id, session.storage_tx, handle, dropped).await;
}

async fn close_all_sessions(state: &HttpState) {
    let entries: Vec<_> = state.sessions.lock().await.drain().collect();
    for (_, SessionEntry { session, handle }) in entries {
        let dropped = session.dropped_messages.load(Ordering::Relaxed);
        finalize_session_entry(session.session_id, session.storage_tx, handle, dropped).await;
    }
}

async fn proxy_request(State(state): State<HttpState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let target = match target_url(&state.target, &parts.uri) {
        Ok(target) => target,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let captures_request = parts.method == Method::POST;
    let is_delete = parts.method == Method::DELETE;
    let key = session_key_for_request(&state, &parts.headers);
    let session = match resolve_session(&state, key.clone()).await {
        Ok(session) => session,
        Err(error) => {
            warn!("failed to open HTTP recording session: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to open recording session",
            )
                .into_response();
        }
    };
    // A provisional (header-less) exchange is one-shot by construction: no
    // future request can ever look up this exact key again, so it must be
    // finalized once this exchange completes or its writer thread and DB
    // connection would leak for the rest of the proxy's lifetime. A DELETE
    // against an established legacy session is the client's explicit
    // termination signal per that protocol era. A DELETE carrying the
    // configured correlation header is also an explicit local recording
    // boundary for a stateless group; the upstream response is still
    // forwarded unchanged (a modern server will normally answer 405).
    let close_after_exchange = matches!(key, LogicalSessionKey::Provisional(_))
        || (is_delete
            && matches!(
                key,
                LogicalSessionKey::Established(_) | LogicalSessionKey::StatelessGroup(_)
            ));

    let headers = end_to_end_headers(&parts.headers, true);
    let (request_tx, request_rx) = tokio_mpsc::channel(REQUEST_FORWARD_CHANNEL_CAPACITY);
    spawn_capture_task(
        &state,
        forward_request_body(body, request_tx, captures_request, session.clone()),
    )
    .await;

    let upstream_body = reqwest::Body::wrap_stream(stream::unfold(request_rx, |mut rx| async {
        rx.recv().await.map(|item| (item, rx))
    }));
    let upstream = state
        .client
        .request(parts.method, target)
        .headers(headers)
        .body(upstream_body)
        .send()
        .await;
    let upstream = match upstream {
        Ok(response) => response,
        Err(error) => {
            warn!("upstream Streamable HTTP request failed: {error}");
            if close_after_exchange {
                close_session(&state, &key).await;
            }
            return (StatusCode::BAD_GATEWAY, "MCP upstream request failed").into_response();
        }
    };

    let status = upstream.status();
    let headers = end_to_end_headers(upstream.headers(), false);
    let capture_kind = response_capture_kind(&headers);
    let mut builder = Response::builder().status(status);
    if let Some(response_headers) = builder.headers_mut() {
        response_headers.extend(headers);
    }

    let response_body = if let Some(capture_kind) = capture_kind {
        let (capture_tx, capture_rx) = tokio_mpsc::channel(RESPONSE_CAPTURE_CHANNEL_CAPACITY);
        let capture_overflow = Arc::new(AtomicBool::new(false));
        let session_for_capture = session.clone();
        let overflow_for_capture = Arc::clone(&capture_overflow);
        let state_for_close = state.clone();
        let key_for_close = key.clone();
        spawn_capture_task(&state, async move {
            capture_response_body(
                capture_rx,
                capture_kind,
                overflow_for_capture,
                session_for_capture,
            )
            .await;
            if close_after_exchange {
                close_session(&state_for_close, &key_for_close).await;
            }
        })
        .await;

        let dropped_messages = Arc::clone(&session.dropped_messages);
        let response_stream = upstream.bytes_stream().map(move |chunk| {
            if let Ok(bytes) = &chunk {
                match capture_tx.try_send(bytes.clone()) {
                    Ok(()) => {}
                    Err(tokio_mpsc::error::TrySendError::Full(_)) => {
                        if !capture_overflow.swap(true, Ordering::Relaxed) {
                            dropped_messages.fetch_add(1, Ordering::Relaxed);
                            warn!("response capture channel full; continuing HTTP forwarding without recording the remaining body");
                        }
                    }
                    Err(tokio_mpsc::error::TrySendError::Closed(_)) => {}
                }
            }
            chunk
        });
        Body::from_stream(response_stream)
    } else {
        if close_after_exchange {
            close_session(&state, &key).await;
        }
        Body::from_stream(upstream.bytes_stream())
    };

    builder.body(response_body).unwrap_or_else(|error| {
        warn!("failed to build proxied HTTP response: {error}");
        (StatusCode::BAD_GATEWAY, "MCP proxy response failed").into_response()
    })
}

async fn spawn_capture_task<F>(state: &HttpState, task: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    state.capture_tasks.lock().await.spawn(task);
}

async fn forward_request_body(
    body: Body,
    sender: tokio_mpsc::Sender<std::result::Result<Bytes, io::Error>>,
    capture: bool,
    session: HttpSession,
) {
    let mut body_stream = body.into_data_stream();
    let mut captured = Vec::new();
    let mut too_large = false;

    while let Some(chunk) = body_stream.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(error) => {
                if capture {
                    session.dropped_messages.fetch_add(1, Ordering::Relaxed);
                }
                let _ = sender.send(Err(io::Error::other(error.to_string()))).await;
                return;
            }
        };

        if sender.send(Ok(bytes.clone())).await.is_err() {
            return;
        }
        if capture && !too_large {
            if captured.len().saturating_add(bytes.len()) <= MAX_FRAME_BYTES {
                captured.extend_from_slice(&bytes);
            } else {
                too_large = true;
                captured.clear();
            }
        }
    }

    if capture {
        if too_large {
            session.dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("request body exceeded {MAX_FRAME_BYTES} bytes; forwarded but not recorded");
        } else if !captured.is_empty() {
            record_message(&session, &captured, Direction::ClientToServer);
        }
    }
}

async fn capture_response_body(
    mut receiver: tokio_mpsc::Receiver<Bytes>,
    capture_kind: ResponseCaptureKind,
    overflowed: Arc<AtomicBool>,
    session: HttpSession,
) {
    match capture_kind {
        ResponseCaptureKind::Json => {
            let mut captured = Vec::new();
            let mut too_large = false;
            while let Some(bytes) = receiver.recv().await {
                if !too_large && !overflowed.load(Ordering::Relaxed) {
                    if captured.len().saturating_add(bytes.len()) <= MAX_FRAME_BYTES {
                        captured.extend_from_slice(&bytes);
                    } else {
                        too_large = true;
                        captured.clear();
                    }
                }
            }
            if !too_large && !overflowed.load(Ordering::Relaxed) && !captured.is_empty() {
                record_message(&session, &captured, Direction::ServerToClient);
            } else if too_large {
                session.dropped_messages.fetch_add(1, Ordering::Relaxed);
                warn!("response JSON body exceeded {MAX_FRAME_BYTES} bytes; forwarded but not recorded");
            }
        }
        ResponseCaptureKind::Sse => {
            let mut decoder = SseDecoder::default();
            while let Some(bytes) = receiver.recv().await {
                if overflowed.load(Ordering::Relaxed) {
                    return;
                }
                for event in decoder.push(&bytes) {
                    record_sse_event(&session, event);
                }
            }
            if !overflowed.load(Ordering::Relaxed) {
                for event in decoder.finish() {
                    record_sse_event(&session, event);
                }
            }
        }
    }
}

fn record_sse_event(session: &HttpSession, event: SseEvent) {
    match event {
        SseEvent::Json(bytes) => record_message(session, &bytes, Direction::ServerToClient),
        SseEvent::Oversized => {
            session.dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("SSE data event exceeded {MAX_FRAME_BYTES} bytes; forwarded but not recorded");
        }
    }
}

fn record_message(session: &HttpSession, bytes: &[u8], direction: Direction) {
    let seq = session.seq.fetch_add(1, Ordering::Relaxed);
    match StreamableHttpTransport.decode_message(bytes, direction, seq, now_ns()) {
        Ok(message) => try_record(
            &session.storage_tx,
            message,
            &session.dropped_messages,
            &session.queue_budget,
        ),
        Err(error) => {
            warn!("forwarded non-JSON Streamable HTTP payload but did not record it: {error}")
        }
    }
}

fn response_capture_kind(headers: &HeaderMap) -> Option<ResponseCaptureKind> {
    let content_type = headers
        .get(CONTENT_TYPE)?
        .to_str()
        .ok()?
        .to_ascii_lowercase();
    if content_type.starts_with("text/event-stream") {
        Some(ResponseCaptureKind::Sse)
    } else if content_type.starts_with("application/json") {
        Some(ResponseCaptureKind::Json)
    } else {
        None
    }
}

fn end_to_end_headers(headers: &HeaderMap, upstream_request: bool) -> HeaderMap {
    let connection_named = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut forwarded = HeaderMap::new();

    for (name, value) in headers {
        if is_hop_by_hop_header(name)
            || connection_named.contains(name.as_str())
            || (upstream_request && name == HOST)
        {
            continue;
        }
        forwarded.append(name.clone(), value.clone());
    }
    forwarded
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

pub(crate) fn parse_target(raw: &str) -> Result<Url> {
    let target = Url::parse(raw).context("--target must be an absolute URL")?;
    if !matches!(target.scheme(), "http" | "https") {
        return Err(anyhow!("--target must use the http or https scheme"));
    }
    if target.host_str().is_none() {
        return Err(anyhow!("--target must include a host"));
    }
    if !target.username().is_empty()
        || target.password().is_some()
        || target.query().is_some()
        || target.fragment().is_some()
    {
        return Err(anyhow!(
            "--target cannot contain user info, query parameters, or a fragment"
        ));
    }
    Ok(target)
}

pub(crate) fn sanitized_target(target: &Url) -> String {
    let mut display = target.clone();
    let _ = display.set_username("");
    let _ = display.set_password(None);
    display.set_query(None);
    display.set_fragment(None);
    display.to_string().trim_end_matches('/').to_string()
}

fn target_url(base: &Url, incoming: &Uri) -> Result<Url> {
    if incoming.path().split('/').any(|segment| segment == "..") {
        return Err(anyhow!(
            "request path cannot contain parent-directory segments"
        ));
    }
    let mut target = base.clone();
    let base_path = base.path().trim_end_matches('/');
    let incoming_path = incoming.path().trim_start_matches('/');
    let path = if incoming_path.is_empty() {
        if base_path.is_empty() {
            "/".to_string()
        } else {
            base_path.to_string()
        }
    } else if base_path.is_empty() || base_path == "/" {
        format!("/{incoming_path}")
    } else {
        format!("{base_path}/{incoming_path}")
    };
    target.set_path(&path);
    target.set_query(incoming.query());
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::{
        end_to_end_headers, parse_target, session_key_for_request, target_url, HttpState,
        LogicalSessionKey, ResponseCaptureKind,
    };
    use axum::http::header::{CONNECTION, HOST};
    use axum::http::{HeaderMap, HeaderValue, Uri};
    use mcptracer_redact::RedactionPolicy;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::task::JoinSet;

    fn test_state() -> HttpState {
        HttpState {
            target: url::Url::parse("https://example.test/mcp").unwrap(),
            client: reqwest::Client::new(),
            db_path: std::path::PathBuf::from(":memory:"),
            client_name: "test".to_string(),
            policy: RedactionPolicy::None,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            provisional_counter: Arc::new(AtomicU64::new(0)),
            stateless_group_header: None,
            capture_tasks: Arc::new(Mutex::new(JoinSet::new())),
        }
    }

    #[test]
    fn session_key_uses_the_established_id_from_the_request_header() {
        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", HeaderValue::from_static("abc-123"));
        let state = test_state();

        let key = session_key_for_request(&state, &headers);

        assert_eq!(key, LogicalSessionKey::Established("abc-123".to_string()));
    }

    #[test]
    fn session_key_ignores_an_empty_session_id_header() {
        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", HeaderValue::from_static(""));
        let state = test_state();

        let key = session_key_for_request(&state, &headers);

        assert!(matches!(key, LogicalSessionKey::Provisional(_)));
    }

    #[test]
    fn session_key_is_a_fresh_provisional_id_per_header_less_request() {
        let headers = HeaderMap::new();
        let state = test_state();

        let first = session_key_for_request(&state, &headers);
        let second = session_key_for_request(&state, &headers);

        assert_ne!(first, second);
        assert!(matches!(first, LogicalSessionKey::Provisional(_)));
        assert!(matches!(second, LogicalSessionKey::Provisional(_)));
    }

    #[test]
    fn two_established_sessions_with_different_ids_are_distinct_keys() {
        let mut first_headers = HeaderMap::new();
        first_headers.insert("mcp-session-id", HeaderValue::from_static("session-a"));
        let mut second_headers = HeaderMap::new();
        second_headers.insert("mcp-session-id", HeaderValue::from_static("session-b"));
        let state = test_state();

        let first = session_key_for_request(&state, &first_headers);
        let second = session_key_for_request(&state, &second_headers);

        assert_ne!(first, second);
    }

    #[test]
    fn configured_header_groups_stateless_requests_without_overriding_session_ids() {
        let mut state = test_state();
        state.stateless_group_header = Some("x-trace-id".parse().unwrap());
        let mut headers = HeaderMap::new();
        headers.insert("x-trace-id", HeaderValue::from_static("trace-secret-value"));

        let first = session_key_for_request(&state, &headers);
        let second = session_key_for_request(&state, &headers);
        assert_eq!(first, second);
        assert_eq!(
            first,
            LogicalSessionKey::StatelessGroup("trace-secret-value".to_string())
        );
        assert!(!super::describe_key(&first).contains("trace-secret-value"));

        headers.insert("mcp-session-id", HeaderValue::from_static("legacy-session"));
        assert_eq!(
            session_key_for_request(&state, &headers),
            LogicalSessionKey::Established("legacy-session".to_string())
        );
    }

    #[test]
    fn response_content_type_selects_json_and_sse_capture() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        assert!(matches!(
            super::response_capture_kind(&headers),
            Some(ResponseCaptureKind::Json)
        ));
        headers.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        assert!(matches!(
            super::response_capture_kind(&headers),
            Some(ResponseCaptureKind::Sse)
        ));
    }

    #[test]
    fn filters_hop_by_hop_headers_but_keeps_mcp_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("x-remove"));
        headers.insert("x-remove", HeaderValue::from_static("no"));
        headers.insert(HOST, HeaderValue::from_static("local-proxy"));
        headers.insert("mcp-session-id", HeaderValue::from_static("server-session"));
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static("2025-06-18"),
        );

        let filtered = end_to_end_headers(&headers, true);

        assert!(!filtered.contains_key(CONNECTION));
        assert!(!filtered.contains_key("x-remove"));
        assert!(!filtered.contains_key(HOST));
        assert_eq!(filtered["mcp-session-id"], "server-session");
        assert_eq!(filtered["mcp-protocol-version"], "2025-06-18");
    }

    #[test]
    fn target_requires_safe_absolute_http_url() {
        assert!(parse_target("https://example.test/mcp").is_ok());
        assert!(parse_target("ftp://example.test/mcp").is_err());
        assert!(parse_target("https://user:secret@example.test/mcp").is_err());
        assert!(parse_target("https://example.test/mcp?token=secret").is_err());
    }

    #[test]
    fn target_path_keeps_configured_endpoint_for_root_requests() {
        let target = parse_target("https://example.test/mcp").unwrap();
        let root = target_url(&target, &"/?trace=1".parse::<Uri>().unwrap()).unwrap();
        let child = target_url(&target, &"/extra".parse::<Uri>().unwrap()).unwrap();

        assert_eq!(root.as_str(), "https://example.test/mcp?trace=1");
        assert_eq!(child.as_str(), "https://example.test/mcp/extra");
    }
}
