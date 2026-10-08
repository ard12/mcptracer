use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST};
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
use tokio::sync::{mpsc as tokio_mpsc, Mutex, OwnedSemaphorePermit, Semaphore};
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

    /// Maximum live recording sessions/writers. Excess traffic still forwards.
    #[arg(long, default_value = "32", value_parser = clap::value_parser!(u32).range(1..=256))]
    pub max_recording_sessions: u32,

    /// Maximum exchanges with active request/response capture.
    #[arg(long, default_value = "64", value_parser = clap::value_parser!(u32).range(1..=1024))]
    pub max_capture_exchanges: u32,

    /// Retire inactive recording sessions after this many seconds.
    #[arg(long, default_value = "300", value_parser = clap::value_parser!(u64).range(1..=86400))]
    pub recording_idle_seconds: u64,
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
        LogicalSessionKey::Established(_) => "established session (value not logged)".to_string(),
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
    _writer_permit: OwnedSemaphorePermit,
    active_exchanges: usize,
    last_activity: Instant,
    retiring: bool,
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
    writer_slots: Arc<Semaphore>,
    capture_slots: Arc<Semaphore>,
    max_recording_sessions: u32,
    max_capture_exchanges: u32,
    recording_idle: Duration,
    omitted_exchanges: Arc<AtomicU64>,
    finalization_failures: Arc<AtomicU64>,
}

/// One recording admission shared by both body directions. The final clone
/// releases the logical-session lease only after all capture handles drop.
/// Its permit remains held through asynchronous cleanup, bounding that work
/// too. Cancellation/drop therefore cannot leak an active lease.
struct CaptureLease {
    state: HttpState,
    key: LogicalSessionKey,
    session: HttpSession,
    permit: Option<OwnedSemaphorePermit>,
    close_after_exchange: bool,
}

impl CaptureLease {
    fn session(&self) -> &HttpSession {
        &self.session
    }
}

impl Drop for CaptureLease {
    fn drop(&mut self) {
        let state = self.state.clone();
        let key = self.key.clone();
        let close = self.close_after_exchange;
        let permit = self.permit.take();
        // This destructor cannot emit more records; its session sender drops
        // on return. Other live capture owners retain their own Arc lease.
        tokio::spawn(async move {
            release_exchange(&state, &key, close).await;
            drop(permit);
        });
    }
}

#[derive(Clone, Copy)]
enum ResponseCaptureKind {
    Json,
    Sse,
}

struct ResponseCaptureTap {
    sender: tokio_mpsc::Sender<Bytes>,
    overflowed: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    complete: Arc<AtomicBool>,
    framed_stream: bool,
    expected_length: Option<u64>,
    observed_length: u64,
}

impl ResponseCaptureTap {
    fn mark_incomplete(&self) {
        if !self.overflowed.swap(true, Ordering::Relaxed) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            warn!("HTTP response capture incomplete; forwarding unchanged");
        }
    }

    fn observe(&mut self, chunk: &std::result::Result<Bytes, reqwest::Error>) {
        if self.overflowed.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(bytes) = chunk {
            self.observed_length = self.observed_length.saturating_add(bytes.len() as u64);
            if let Some(expected) = self.expected_length {
                self.complete
                    .store(self.observed_length == expected, Ordering::Relaxed);
                if self.observed_length > expected {
                    self.mark_incomplete();
                }
            }
            if self.sender.try_send(bytes.clone()).is_err() {
                self.mark_incomplete();
            }
        } else {
            self.mark_incomplete();
        }
    }
}

impl Drop for ResponseCaptureTap {
    fn drop(&mut self) {
        // Runs before the sender field closes the channel, so its consumer
        // cannot mistake client disconnect/body cancellation for clean EOF.
        if !self.complete.load(Ordering::Relaxed)
            && (!self.framed_stream || self.expected_length.is_some())
        {
            self.mark_incomplete();
        }
    }
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
        writer_slots: Arc::new(Semaphore::new(args.max_recording_sessions as usize)),
        capture_slots: Arc::new(Semaphore::new(args.max_capture_exchanges as usize)),
        max_recording_sessions: args.max_recording_sessions,
        max_capture_exchanges: args.max_capture_exchanges,
        recording_idle: Duration::from_secs(args.recording_idle_seconds),
        omitted_exchanges: Arc::new(AtomicU64::new(0)),
        finalization_failures: Arc::new(AtomicU64::new(0)),
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
    let reaper_state = state.clone();
    let (reaper_stop_tx, mut reaper_stop_rx) = tokio::sync::oneshot::channel::<()>();
    let reaper = tokio::spawn(async move {
        let cadence = reaper_state.recording_idle.min(Duration::from_secs(30));
        let mut interval = tokio::time::interval(cadence);
        loop {
            tokio::select! {
                _ = &mut reaper_stop_rx => break,
                _ = interval.tick() => retire_idle_sessions(&reaper_state).await,
            }
        }
    });
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(wait_for_shutdown())
        .await
        .context("Streamable HTTP proxy failed");
    // Finish any retirement batch before stopping. Aborting while it owns
    // removed entries would detach writers without finalizing their records.
    let _ = reaper_stop_tx.send(());
    if reaper.await.is_err() {
        state.finalization_failures.fetch_add(1, Ordering::Relaxed);
    }

    wait_for_capture_tasks(&state).await;
    let capture_permits = state
        .capture_slots
        .clone()
        .acquire_many_owned(state.max_capture_exchanges)
        .await?;
    close_all_sessions(&state).await;
    let writer_permits = state
        .writer_slots
        .clone()
        .acquire_many_owned(state.max_recording_sessions)
        .await?;
    drop((capture_permits, writer_permits));
    served?;
    let omitted = state.omitted_exchanges.load(Ordering::Relaxed);
    let failed = state.finalization_failures.load(Ordering::Relaxed);
    if omitted > 0 || failed > 0 {
        return Err(anyhow!("HTTP recording incomplete: {omitted} exchange(s) omitted; {failed} session finalization failure(s); forwarded traffic was not rejected by recording admission"));
    }
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

async fn resolve_session(
    state: &HttpState,
    key: LogicalSessionKey,
    close_after_exchange: bool,
) -> Result<HttpSession> {
    // Admission must not wait for unrelated SQLite writers to finish. Each
    // detached retirement retains its writer permit through result accounting,
    // so cleanup stays bounded and shutdown still waits for it.
    for entry in take_idle_sessions(state).await {
        let state = state.clone();
        tokio::spawn(async move { finalize_session_entry(&state, entry).await });
    }
    let mut sessions = state.sessions.lock().await;
    if let Some(entry) = sessions.get_mut(&key) {
        if entry.retiring {
            return Err(anyhow!("recording session is being retired"));
        }
        entry.active_exchanges += 1;
        entry.retiring |= close_after_exchange;
        return Ok(entry.session.clone());
    }
    let writer_permit = state
        .writer_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| anyhow!("recording session limit reached"))?;
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
            _writer_permit: writer_permit,
            active_exchanges: 1,
            last_activity: Instant::now(),
            retiring: close_after_exchange,
        },
    );
    Ok(session)
}

async fn finalize_session_entry(state: &HttpState, entry: SessionEntry) {
    let SessionEntry {
        session,
        handle,
        _writer_permit,
        ..
    } = entry;
    let result = tokio::task::spawn_blocking(move || {
        let dropped = session.dropped_messages.load(Ordering::Relaxed);
        finish_storage_writer(session.storage_tx, handle, now_ns(), dropped)
    })
    .await;
    if !matches!(result, Ok(Ok(()))) {
        state.finalization_failures.fetch_add(1, Ordering::Relaxed);
        warn!("failed to finalize HTTP recording session; capture is incomplete");
    }
    // Shutdown's acquisition of all writer permits must also synchronize the
    // failure counter, not merely the underlying thread's exit.
    drop(_writer_permit);
}

async fn release_exchange(state: &HttpState, key: &LogicalSessionKey, close: bool) {
    let entry = {
        let mut sessions = state.sessions.lock().await;
        let Some(entry) = sessions.get_mut(key) else {
            return;
        };
        entry.active_exchanges = entry.active_exchanges.saturating_sub(1);
        entry.last_activity = Instant::now();
        entry.retiring |= close;
        if entry.active_exchanges == 0 && entry.retiring {
            sessions.remove(key)
        } else {
            None
        }
    };
    if let Some(entry) = entry {
        finalize_session_entry(state, entry).await;
    }
}

async fn take_idle_sessions(state: &HttpState) -> Vec<SessionEntry> {
    let mut sessions = state.sessions.lock().await;
    let keys: Vec<_> = sessions
        .iter()
        .filter(|(_, entry)| {
            entry.active_exchanges == 0 && entry.last_activity.elapsed() >= state.recording_idle
        })
        .map(|(key, _)| key.clone())
        .collect();
    keys.into_iter()
        .filter_map(|key| sessions.remove(&key))
        .collect::<Vec<_>>()
}

async fn retire_idle_sessions(state: &HttpState) {
    for entry in take_idle_sessions(state).await {
        finalize_session_entry(state, entry).await;
    }
}

async fn close_all_sessions(state: &HttpState) {
    let entries: Vec<_> = state.sessions.lock().await.drain().collect();
    for (_, entry) in entries {
        finalize_session_entry(state, entry).await;
    }
}

async fn admit_capture(
    state: &HttpState,
    key: LogicalSessionKey,
    close_after_exchange: bool,
) -> Option<Arc<CaptureLease>> {
    let permit = state.capture_slots.clone().try_acquire_owned().ok();
    if let Some(permit) = permit {
        if let Ok(session) = resolve_session(state, key.clone(), close_after_exchange).await {
            return Some(Arc::new(CaptureLease {
                state: state.clone(),
                key,
                session,
                permit: Some(permit),
                close_after_exchange,
            }));
        }
    }
    state.omitted_exchanges.fetch_add(1, Ordering::Relaxed);
    let retired = {
        let mut sessions = state.sessions.lock().await;
        if let Some(entry) = sessions.get_mut(&key) {
            entry
                .session
                .dropped_messages
                .fetch_add(1, Ordering::Relaxed);
            entry.retiring |= close_after_exchange;
            if entry.retiring && entry.active_exchanges == 0 {
                sessions.remove(&key)
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some(entry) = retired {
        let state = state.clone();
        // Owned writer permit bounds finalization even without capture admission.
        tokio::spawn(async move {
            finalize_session_entry(&state, entry).await;
        });
    }
    warn!("HTTP recording admission unavailable; forwarding exchange without capture");
    None
}

async fn proxy_request(State(state): State<HttpState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let target = match target_url(&state.target, &parts.uri) {
        Ok(target) => target,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    let captures_request = parts.method == Method::POST;
    let request_length = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let is_delete = parts.method == Method::DELETE;
    let key = session_key_for_request(&state, &parts.headers);
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
    let lease = admit_capture(&state, key, close_after_exchange).await;

    let headers = end_to_end_headers(&parts.headers, true);
    let upstream_body = if let Some(lease) = &lease {
        let (request_tx, request_rx) = tokio_mpsc::channel(REQUEST_FORWARD_CHANNEL_CAPACITY);
        let request_lease = Arc::clone(lease);
        let session = request_lease.session().clone();
        spawn_capture_task(&state, async move {
            forward_request_body(body, request_tx, captures_request, session, request_length).await;
            drop(request_lease);
        })
        .await;
        reqwest::Body::wrap_stream(stream::unfold(request_rx, |mut rx| async {
            rx.recv().await.map(|item| (item, rx))
        }))
    } else {
        // No capture buffers, writer, or dedicated capture task is allocated
        // for rejected admission. Ordinary transport backpressure still applies.
        reqwest::Body::wrap_stream(
            body.into_data_stream()
                .map(|chunk| chunk.map_err(|error| io::Error::other(error.to_string()))),
        )
    };
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
            // Query parameters can contain credentials; omit the request URL.
            warn!(
                "upstream Streamable HTTP request failed: {}",
                error.without_url()
            );
            return (StatusCode::BAD_GATEWAY, "MCP upstream request failed").into_response();
        }
    };

    let status = upstream.status();
    let headers = end_to_end_headers(upstream.headers(), false);
    let capture_kind = response_capture_kind(&headers);
    let response_length = upstream.content_length();
    let mut builder = Response::builder().status(status);
    if let Some(response_headers) = builder.headers_mut() {
        response_headers.extend(headers);
    }

    let response_body = if let (Some(capture_kind), Some(lease)) = (capture_kind, &lease) {
        let (capture_tx, capture_rx) = tokio_mpsc::channel(RESPONSE_CAPTURE_CHANNEL_CAPACITY);
        let capture_overflow = Arc::new(AtomicBool::new(false));
        let response_lease = Arc::clone(lease);
        let session_for_capture = response_lease.session().clone();
        let overflow_for_capture = Arc::clone(&capture_overflow);
        spawn_capture_task(&state, async move {
            capture_response_body(
                capture_rx,
                capture_kind,
                overflow_for_capture,
                session_for_capture,
            )
            .await;
            drop(response_lease);
        })
        .await;

        let tap = ResponseCaptureTap {
            sender: capture_tx,
            overflowed: capture_overflow,
            dropped: Arc::clone(&lease.session().dropped_messages),
            complete: Arc::new(AtomicBool::new(response_length == Some(0))),
            framed_stream: matches!(capture_kind, ResponseCaptureKind::Sse),
            expected_length: response_length,
            observed_length: 0,
        };
        let response_stream = stream::unfold(
            (Box::pin(upstream.bytes_stream()), tap),
            |(mut stream, mut tap)| async move {
                match stream.next().await {
                    Some(chunk) => {
                        tap.observe(&chunk);
                        Some((chunk, (stream, tap)))
                    }
                    None => {
                        tap.complete.store(
                            tap.expected_length
                                .is_none_or(|expected| expected == tap.observed_length),
                            Ordering::Relaxed,
                        );
                        None
                    }
                }
            },
        );
        Body::from_stream(response_stream)
    } else {
        Body::from_stream(upstream.bytes_stream().map(move |chunk| {
            // Retain admission until this non-captured response is consumed or
            // dropped, including 202/DELETE/non-JSON bodies.
            let _ = &lease;
            chunk
        }))
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
    let mut tasks = state.capture_tasks.lock().await;
    // JoinSet retains completed entries until joined. Reap without waiting
    // before admitting new work, rather than accumulating them until shutdown.
    while let Some(result) = tasks.try_join_next() {
        if let Err(error) = result {
            warn!("HTTP capture task failed: {error}");
        }
    }
    tasks.spawn(task);
}

async fn forward_request_body(
    body: Body,
    sender: tokio_mpsc::Sender<std::result::Result<Bytes, io::Error>>,
    capture: bool,
    session: HttpSession,
    expected_length: Option<u64>,
) {
    let mut body_stream = body.into_data_stream();
    let mut captured = Vec::new();
    let mut too_large = false;
    // Reserve before request bytes can reach the upstream. It may answer
    // after the last Content-Length byte but before our body stream yields
    // EOF. The shared writer then restores this order across capture tasks.
    let stamp = capture.then(|| (session.seq.fetch_add(1, Ordering::Relaxed), now_ns()));
    let mut forwarded_length = 0_u64;

    while expected_length != Some(forwarded_length) {
        let chunk = tokio::select! {
            _ = sender.closed() => {
                if capture {
                    session.dropped_messages.fetch_add(1, Ordering::Relaxed);
                }
                return;
            },
            chunk = body_stream.next() => chunk,
        };
        let Some(chunk) = chunk else {
            break;
        };
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
            if capture {
                session.dropped_messages.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        forwarded_length = forwarded_length.saturating_add(bytes.len() as u64);
        if capture && !too_large {
            if captured.len().saturating_add(bytes.len()) <= MAX_FRAME_BYTES {
                captured.extend_from_slice(&bytes);
            } else {
                too_large = true;
                captured.clear();
            }
        }
        if expected_length == Some(forwarded_length) {
            // Content-Length is authoritative framing. Reqwest may close its
            // receiver after this final chunk without asking us for another
            // EOF poll. Do not turn successful forwarding into capture loss.
            break;
        }
    }

    if capture {
        if too_large {
            session.dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("request body exceeded {MAX_FRAME_BYTES} bytes; forwarded but not recorded");
        } else if expected_length.is_some_and(|expected| expected != forwarded_length) {
            session.dropped_messages.fetch_add(1, Ordering::Relaxed);
        } else if let Some((seq, timestamp)) = stamp {
            record_message_at(
                &session,
                &captured,
                Direction::ClientToServer,
                seq,
                timestamp,
            );
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
        SseEvent::Incomplete => {
            session.dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("SSE stream ended inside an event; partial event not recorded");
        }
    }
}

fn record_message(session: &HttpSession, bytes: &[u8], direction: Direction) {
    let seq = session.seq.fetch_add(1, Ordering::Relaxed);
    record_message_at(session, bytes, direction, seq, now_ns());
}

fn record_message_at(
    session: &HttpSession,
    bytes: &[u8],
    direction: Direction,
    seq: u64,
    timestamp: i64,
) {
    match StreamableHttpTransport.decode_message(bytes, direction, seq, timestamp) {
        Ok(message) => try_record(
            &session.storage_tx,
            message,
            &session.dropped_messages,
            &session.queue_budget,
        ),
        Err(error) => {
            session.dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("forwarded non-JSON Streamable HTTP payload but did not record it: {error}");
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
            writer_slots: Arc::new(tokio::sync::Semaphore::new(32)),
            capture_slots: Arc::new(tokio::sync::Semaphore::new(64)),
            max_recording_sessions: 32,
            max_capture_exchanges: 64,
            recording_idle: std::time::Duration::from_secs(300),
            omitted_exchanges: Arc::new(AtomicU64::new(0)),
            finalization_failures: Arc::new(AtomicU64::new(0)),
        }
    }

    fn test_session() -> (
        super::HttpSession,
        std::sync::mpsc::Receiver<super::StorageEvent>,
    ) {
        let (storage_tx, storage_rx) = std::sync::mpsc::sync_channel(1);
        (
            super::HttpSession {
                session_id: "dummy".to_string(),
                storage_tx,
                seq: Arc::new(AtomicU64::new(0)),
                dropped_messages: Arc::new(AtomicU64::new(0)),
                queue_budget: Arc::new(super::StorageQueueBudget::new(1024)),
            },
            storage_rx,
        )
    }

    fn limited_state(writers: u32, captures: u32) -> HttpState {
        let mut state = test_state();
        state.db_path =
            std::env::temp_dir().join(format!("mcptracer-http-limits-{}.db", uuid::Uuid::new_v4()));
        state.writer_slots = Arc::new(tokio::sync::Semaphore::new(writers as usize));
        state.capture_slots = Arc::new(tokio::sync::Semaphore::new(captures as usize));
        state.max_recording_sessions = writers;
        state.max_capture_exchanges = captures;
        state.recording_idle = std::time::Duration::from_secs(1);
        state
    }

    async fn await_capture_cleanup(state: &HttpState, permits: u32) {
        let permits = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.capture_slots.clone().acquire_many_owned(permits),
        )
        .await
        .unwrap()
        .unwrap();
        drop(permits);
    }

    #[tokio::test]
    async fn distinct_keys_bound_writers_without_mixing_existing_clients() {
        let state = limited_state(2, 4);
        let akey = LogicalSessionKey::Established("a".to_string());
        let bkey = LogicalSessionKey::Established("b".to_string());
        let a = super::admit_capture(&state, akey.clone(), false)
            .await
            .unwrap();
        let b = super::admit_capture(&state, bkey, false).await.unwrap();
        assert!(super::admit_capture(
            &state,
            LogicalSessionKey::Established("c".to_string()),
            false
        )
        .await
        .is_none());
        let again = super::admit_capture(&state, akey, false).await.unwrap();
        assert_eq!(a.session().session_id, again.session().session_id);
        assert_ne!(a.session().session_id, b.session().session_id);
        assert_eq!(state.sessions.lock().await.len(), 2);
        assert_eq!(state.writer_slots.available_permits(), 0);
        for (lease, owner) in [(&a, "a"), (&b, "b")] {
            let bytes = serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","method":"notifications/test","params":{"owner":owner}})).unwrap();
            super::record_message(
                lease.session(),
                &bytes,
                mcptracer_protocol::Direction::ClientToServer,
            );
        }
        let aid = a.session().session_id.clone();
        let bid = b.session().session_id.clone();
        drop((a, b, again));
        await_capture_cleanup(&state, 4).await;
        super::close_all_sessions(&state).await;
        let store = mcptracer_storage::Store::open(&state.db_path).unwrap();
        let amsg = store.get_messages(&aid).unwrap();
        let bmsg = store.get_messages(&bid).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&amsg[0].payload).unwrap()["params"]["owner"],
            "a"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&bmsg[0].payload).unwrap()["params"]["owner"],
            "b"
        );
        assert_eq!(state.writer_slots.available_permits(), 2);
        assert_eq!(
            state
                .omitted_exchanges
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn capture_pressure_marks_existing_evidence_and_drop_reclaims_admission() {
        let state = limited_state(2, 1);
        let key = LogicalSessionKey::Established("a".to_string());
        let a = super::admit_capture(&state, key.clone(), true)
            .await
            .unwrap();
        let id = a.session().session_id.clone();
        assert!(super::admit_capture(&state, key, false).await.is_none());
        assert_eq!(
            a.session()
                .dropped_messages
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(state.capture_slots.available_permits(), 0);
        drop(a);
        await_capture_cleanup(&state, 1).await;
        assert!(state.sessions.lock().await.is_empty());
        assert_eq!(state.writer_slots.available_permits(), 2);
        let store = mcptracer_storage::Store::open(&state.db_path).unwrap();
        let summary = store.get_session_summary(&id).unwrap();
        assert!(summary.ended_at_ns.is_some());
        assert_eq!(summary.dropped_messages, 1);
    }

    #[tokio::test]
    async fn idle_retirement_preserves_active_capture_and_opens_a_new_recording_on_reuse() {
        let state = limited_state(2, 4);
        let akey = LogicalSessionKey::Established("active".to_string());
        let bkey = LogicalSessionKey::Established("idle".to_string());
        let a = super::admit_capture(&state, akey.clone(), false)
            .await
            .unwrap();
        let b = super::admit_capture(&state, bkey.clone(), false)
            .await
            .unwrap();
        let old_id = b.session().session_id.clone();
        drop(b);
        await_capture_cleanup(&state, 3).await;
        {
            let mut sessions = state.sessions.lock().await;
            for entry in sessions.values_mut() {
                entry.last_activity = std::time::Instant::now() - std::time::Duration::from_secs(2);
            }
        }
        super::retire_idle_sessions(&state).await;
        assert_eq!(state.sessions.lock().await.len(), 1);
        assert!(state.sessions.lock().await.contains_key(&akey));
        let b = super::admit_capture(&state, bkey, false).await.unwrap();
        assert_ne!(b.session().session_id, old_id);
        let store = mcptracer_storage::Store::open(&state.db_path).unwrap();
        assert!(store
            .get_session_summary(&old_id)
            .unwrap()
            .ended_at_ns
            .is_some());
        drop((a, b));
        await_capture_cleanup(&state, 4).await;
        super::close_all_sessions(&state).await;
    }

    #[tokio::test]
    async fn delete_boundary_waits_for_overlapping_capture_before_closing() {
        let state = limited_state(1, 3);
        let key = LogicalSessionKey::Established("same".to_string());
        let active = super::admit_capture(&state, key.clone(), false)
            .await
            .unwrap();
        let deletion = super::admit_capture(&state, key.clone(), true)
            .await
            .unwrap();
        let id = active.session().session_id.clone();
        drop(deletion);
        await_capture_cleanup(&state, 2).await;
        assert!(state.sessions.lock().await.get(&key).unwrap().retiring);
        assert!(super::admit_capture(&state, key, false).await.is_none());
        assert!(mcptracer_storage::Store::open(&state.db_path)
            .unwrap()
            .get_session_summary(&id)
            .unwrap()
            .ended_at_ns
            .is_none());
        drop(active);
        await_capture_cleanup(&state, 3).await;
        assert!(state.sessions.lock().await.is_empty());
        assert!(mcptracer_storage::Store::open(&state.db_path)
            .unwrap()
            .get_session_summary(&id)
            .unwrap()
            .ended_at_ns
            .is_some());
    }

    #[tokio::test]
    async fn rejected_delete_retires_overlapping_recording_before_key_reuse() {
        let state = limited_state(1, 1);
        let key = LogicalSessionKey::Established("same".to_string());
        let active = super::admit_capture(&state, key.clone(), false)
            .await
            .unwrap();
        let old_id = active.session().session_id.clone();
        assert!(super::admit_capture(&state, key.clone(), true)
            .await
            .is_none());
        assert!(state.sessions.lock().await.get(&key).unwrap().retiring);
        drop(active);
        await_capture_cleanup(&state, 1).await;
        let store = mcptracer_storage::Store::open(&state.db_path).unwrap();
        let old = store.get_session_summary(&old_id).unwrap();
        assert!(old.ended_at_ns.is_some());
        assert_eq!(old.dropped_messages, 1);
        assert_eq!(
            state
                .finalization_failures
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let reused = super::admit_capture(&state, key, true).await.unwrap();
        assert_ne!(reused.session().session_id, old_id);
        drop(reused);
        await_capture_cleanup(&state, 1).await;
        assert_eq!(state.writer_slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn rejected_capture_forwards_exact_status_headers_and_body_without_capture_tasks() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(axum::routing::any(
            |body: axum::body::Bytes| async move {
                (
                    axum::http::StatusCode::CREATED,
                    [("x-dummy", "unchanged")],
                    body,
                )
            },
        ));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut state = limited_state(1, 1);
        state.target = url::Url::parse(&format!("http://{address}/")).unwrap();
        let held = super::admit_capture(
            &state,
            LogicalSessionKey::Established("held".to_string()),
            true,
        )
        .await
        .unwrap();
        let bytes = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}";
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/")
            .header("mcp-session-id", "rejected")
            .body(axum::body::Body::from(bytes.as_slice()))
            .unwrap();
        let response = super::proxy_request(axum::extract::State(state.clone()), request).await;
        assert_eq!(response.status(), axum::http::StatusCode::CREATED);
        assert_eq!(response.headers()["x-dummy"], "unchanged");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
            bytes.as_slice()
        );
        assert_eq!(state.sessions.lock().await.len(), 1);
        assert!(state.capture_tasks.lock().await.is_empty());
        assert_eq!(
            state
                .omitted_exchanges
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        drop(held);
        await_capture_cleanup(&state, 1).await;
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn expired_paused_writer_cannot_delay_uncaptured_forwarding() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(axum::routing::any(
            |body: axum::body::Bytes| async move {
                (
                    axum::http::StatusCode::CREATED,
                    [("x-dummy", "unchanged")],
                    body,
                )
            },
        ));
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut state = limited_state(1, 2);
        state.target = url::Url::parse(&format!("http://{address}/")).unwrap();
        let old_key = LogicalSessionKey::Established("expired".to_string());
        let old = super::admit_capture(&state, old_key.clone(), false)
            .await
            .unwrap();
        let old_id = old.session().session_id.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        old.session()
            .storage_tx
            .send(super::StorageEvent::TestBarrier {
                entered_tx,
                release_rx,
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while entered_rx.try_recv().is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(old);
        await_capture_cleanup(&state, 2).await;
        state
            .sessions
            .lock()
            .await
            .get_mut(&old_key)
            .unwrap()
            .last_activity = std::time::Instant::now() - std::time::Duration::from_secs(2);
        let bytes = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}";
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/")
            .header("mcp-session-id", "new")
            .body(axum::body::Body::from(bytes.as_slice()))
            .unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            super::proxy_request(axum::extract::State(state.clone()), request),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::CREATED);
        assert_eq!(response.headers()["x-dummy"], "unchanged");
        assert_eq!(
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap(),
            bytes.as_slice()
        );
        // The old writer is still paused, but neither capture admission nor
        // exact byte forwarding waited for its close acknowledgment.
        assert!(state.sessions.lock().await.is_empty());
        assert_eq!(state.writer_slots.available_permits(), 0);
        assert_eq!(
            state
                .omitted_exchanges
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(state.capture_tasks.lock().await.is_empty());
        drop(release_tx);
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.writer_slots.clone().acquire_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            state
                .finalization_failures
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(mcptracer_storage::Store::open(&state.db_path)
            .unwrap()
            .get_session_summary(&old_id)
            .unwrap()
            .ended_at_ns
            .is_some());
        drop(permit);
        server.abort();
        let _ = server.await;
    }

    #[test]
    fn response_exact_content_length_does_not_require_an_extra_eof_poll() {
        let bytes = axum::body::Bytes::from_static(b"dummy");
        for (expected, observed, incomplete) in [
            (Some(0), None, false),
            (Some(5), Some(bytes.clone()), false),
            (Some(6), Some(bytes.clone()), true),
            (None, Some(bytes.clone()), true),
        ] {
            let (session, _storage) = test_session();
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut tap = super::ResponseCaptureTap {
                sender,
                overflowed: Arc::clone(&overflowed),
                dropped: Arc::clone(&session.dropped_messages),
                complete: Arc::new(std::sync::atomic::AtomicBool::new(expected == Some(0))),
                framed_stream: false,
                expected_length: expected,
                observed_length: 0,
            };
            if let Some(bytes) = observed {
                tap.observe(&Ok(bytes));
            }
            drop(tap); // Downstream framing may finish without polling None.
            assert_eq!(
                overflowed.load(std::sync::atomic::Ordering::Relaxed),
                incomplete
            );
            assert_eq!(
                session
                    .dropped_messages
                    .load(std::sync::atomic::Ordering::Relaxed),
                u64::from(incomplete)
            );
            drop(receiver);
        }
    }

    #[tokio::test]
    async fn exact_request_length_finishes_even_when_body_never_yields_eof() {
        let (mut session, storage) = test_session();
        // One valid event includes the writer's fixed accounting overhead.
        session.queue_budget = Arc::new(super::StorageQueueBudget::new(2048));
        let bytes = axum::body::Bytes::from_static(
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}",
        );
        let length = bytes.len() as u64;
        let first = futures_util::stream::once(async move { Ok::<_, std::io::Error>(bytes) });
        let pending = futures_util::stream::pending::<Result<axum::body::Bytes, std::io::Error>>();
        let body = axum::body::Body::from_stream(futures_util::StreamExt::chain(first, pending));
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let forwarding = tokio::spawn(super::forward_request_body(
            body,
            sender,
            true,
            session.clone(),
            Some(length),
        ));
        assert_eq!(receiver.recv().await.unwrap().unwrap().len() as u64, length);
        drop(receiver);
        tokio::time::timeout(std::time::Duration::from_secs(1), forwarding)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            session
                .dropped_messages
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(matches!(
            storage.try_recv().unwrap(),
            super::StorageEvent::Message { .. }
        ));
    }

    #[tokio::test]
    async fn subscription_stop_preserves_delimited_events_but_drops_partial_frames() {
        let event = b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/dummy\"}";
        for (suffix, complete_events, drops) in [
            (b"\n\n".as_slice(), 1, 0),
            (b"".as_slice(), 0, 1),
            (b"\n".as_slice(), 0, 1),
            (b"\n\ndata: {\"jsonrpc\":\"2.0\"}".as_slice(), 1, 1),
        ] {
            let (mut session, storage) = test_session();
            session.queue_budget = Arc::new(super::StorageQueueBudget::new(4096));
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut tap = super::ResponseCaptureTap {
                sender,
                overflowed: Arc::clone(&overflowed),
                dropped: Arc::clone(&session.dropped_messages),
                complete: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                framed_stream: true,
                expected_length: None,
                observed_length: 0,
            };
            tap.observe(&Ok(axum::body::Bytes::from(
                [event.as_slice(), suffix].concat(),
            )));
            drop(tap); // Caller closes an indefinite subscription.
            super::capture_response_body(
                receiver,
                ResponseCaptureKind::Sse,
                overflowed,
                session.clone(),
            )
            .await;
            assert_eq!(
                session
                    .dropped_messages
                    .load(std::sync::atomic::Ordering::Relaxed),
                drops
            );
            assert_eq!(storage.try_iter().count(), complete_events);
        }
    }

    #[tokio::test]
    async fn response_cancellation_is_incomplete_before_capture_channel_closes() {
        let (session, _storage) = test_session();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let overflowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let complete = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tap = super::ResponseCaptureTap {
            sender,
            overflowed: Arc::clone(&overflowed),
            dropped: Arc::clone(&session.dropped_messages),
            complete: Arc::clone(&complete),
            framed_stream: false,
            expected_length: None,
            observed_length: 0,
        };
        let consumer = tokio::spawn(super::capture_response_body(
            receiver,
            ResponseCaptureKind::Json,
            Arc::clone(&overflowed),
            session.clone(),
        ));
        tap.sender
            .try_send(axum::body::Bytes::from_static(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}",
            ))
            .unwrap();
        drop(tap); // A valid prefix is not proof that its HTTP body reached EOF.
        consumer.await.unwrap();
        assert!(overflowed.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            session
                .dropped_messages
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn response_before_request_eof_preserves_matched_tool_schema() {
        let mut state = limited_state(1, 1);
        state.policy = RedactionPolicy::Default;
        let lease = super::admit_capture(&state, LogicalSessionKey::Provisional(0), true)
            .await
            .unwrap();
        let id = lease.session().session_id.clone();
        let (eof_tx, eof_rx) = tokio::sync::oneshot::channel::<()>();
        let first = futures_util::stream::once(async {
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}",
            ))
        });
        let delayed_eof = futures_util::stream::once(async move {
            eof_rx.await.unwrap();
            Ok::<_, std::io::Error>(axum::body::Bytes::new())
        });
        let body =
            axum::body::Body::from_stream(futures_util::StreamExt::chain(first, delayed_eof));
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        let session = lease.session().clone();
        let forward = tokio::spawn(super::forward_request_body(
            body, sender, true, session, None,
        ));
        assert!(!receiver.recv().await.unwrap().unwrap().is_empty());
        super::record_message(lease.session(), b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[{\"name\":\"dummy\",\"inputSchema\":{\"properties\":{\"api_key\":{\"type\":\"string\"}}}}]}}", mcptracer_protocol::Direction::ServerToClient);
        eof_tx.send(()).unwrap();
        forward.await.unwrap();
        drop(lease);
        await_capture_cleanup(&state, 1).await;
        let store = mcptracer_storage::Store::open(&state.db_path).unwrap();
        let messages = store.get_messages(&id).unwrap();
        assert_eq!(messages[0].method.as_deref(), Some("tools/list"));
        let response: serde_json::Value = serde_json::from_str(&messages[1].payload).unwrap();
        assert_eq!(
            response["result"]["tools"][0]["inputSchema"]["properties"]["api_key"],
            serde_json::json!({"type":"string"})
        );
        assert!(crate::session_health::inspect_session(&store, &id)
            .unwrap()
            .report
            .is_healthy());
    }

    #[tokio::test]
    async fn failed_session_creation_releases_both_admission_permits() {
        let mut state = limited_state(1, 1);
        state.db_path = std::env::temp_dir(); // A directory is not a database.
        assert!(
            super::admit_capture(&state, LogicalSessionKey::Provisional(0), true)
                .await
                .is_none()
        );
        assert_eq!(state.writer_slots.available_permits(), 1);
        assert_eq!(state.capture_slots.available_permits(), 1);
        assert!(state.sessions.lock().await.is_empty());
        assert_eq!(
            state
                .omitted_exchanges
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn recording_admission_flags_reject_zero_and_out_of_range_limits() {
        use clap::Parser;
        for (flag, value) in [
            ("--max-recording-sessions", "0"),
            ("--max-recording-sessions", "257"),
            ("--max-capture-exchanges", "0"),
            ("--max-capture-exchanges", "1025"),
            ("--recording-idle-seconds", "0"),
        ] {
            assert!(crate::Cli::try_parse_from([
                "mcptracer",
                "record-http",
                "--target",
                "http://127.0.0.1/",
                flag,
                value
            ])
            .is_err());
        }
    }

    #[tokio::test]
    async fn completed_capture_tasks_are_reaped_before_new_work() {
        let state = test_state();
        let completed = state.capture_tasks.lock().await.spawn(async {});
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !completed.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state.capture_tasks.lock().await.len(), 1);

        super::spawn_capture_task(&state, std::future::pending()).await;
        assert_eq!(state.capture_tasks.lock().await.len(), 1);
        state.capture_tasks.lock().await.abort_all();
        super::wait_for_capture_tasks(&state).await;
        assert!(state.capture_tasks.lock().await.is_empty());
    }

    #[test]
    fn failed_http_decode_marks_the_recording_incomplete() {
        let (session, storage_rx) = test_session();
        super::record_message(
            &session,
            b"not JSON",
            mcptracer_protocol::Direction::ClientToServer,
        );
        assert_eq!(
            session
                .dropped_messages
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(storage_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn idle_request_body_releases_capture_when_upstream_receiver_closes() {
        let (session, _storage_rx) = test_session();
        let body = axum::body::Body::from_stream(futures_util::stream::pending::<
            Result<axum::body::Bytes, std::io::Error>,
        >());
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            super::forward_request_body(body, sender, true, session.clone(), None),
        )
        .await
        .unwrap();
        assert_eq!(
            session
                .dropped_messages
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
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
