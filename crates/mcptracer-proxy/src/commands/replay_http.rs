//! `mcptracer replay-http`: re-send a recorded Streamable HTTP session's
//! client-originated traffic against a live `--target`, capturing a new
//! session for comparison (T-75). MCPTracer never records
//! `Authorization`/session headers (see `record_http`'s `LogicalSessionKey`
//! doc comment), so runtime credentials must be supplied separately via
//! repeatable `--header` flags. Legacy recordings let the target assign a new
//! `Mcp-Session-Id`; self-describing 2026-07-28 requests instead have their
//! required standard routing headers reconstructed from body metadata.
//!
//! Scope: this replays the direct HTTP response to each POST (JSON or SSE) as
//! `record-http` captures it, including modern `subscriptions/listen` streams
//! that are causally tied to later captured requests. A legacy standalone GET
//! SSE stream carrying server-initiated messages remains outside this command's
//! scope — see `docs/spec/http-replay.md` for the disclosed boundary.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use clap::Args;
use mcptracer_model::plan::{driven_messages, tool_is_allowed};
use mcptracer_protocol::{Direction, McpMessage, StreamableHttpTransport, Transport};
use mcptracer_redact::{RedactionPolicy, Redactor};
use mcptracer_storage::{Store, StoredMessage};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::redirect::Policy as RedirectPolicy;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use url::Url;

use crate::commands::record_http::{parse_target, sanitized_target};
use crate::commands::replay::{warn_redacted_replay, warn_side_effects};
use crate::session_health::require_healthy_session;
use crate::session_writer::{
    finish_storage_writer, now_ns, spawn_storage_writer, try_record, StorageEvent,
    StorageQueueBudget, MAX_FRAME_BYTES, STORAGE_QUEUE_MAX_BYTES,
};
use crate::sse::{SseDecoder, SseEvent};

const RESERVED_HEADERS: &[&str] = &[
    "host",
    "content-type",
    "content-length",
    "mcp-session-id",
    "mcp-protocol-version",
    "mcp-method",
    "mcp-name",
    "connection",
    "transfer-encoding",
];

#[derive(Args)]
pub struct ReplayHttpArgs {
    /// Id (or unique prefix) of the recorded Streamable HTTP session to
    /// replay.
    pub session_id: String,

    /// Absolute live Streamable HTTP endpoint URL to replay against.
    #[arg(long)]
    pub target: String,

    /// Extra header to send with every replayed request, in `Name: value`
    /// form (repeatable). MCPTracer never records `Authorization` or
    /// session headers, so runtime credentials must be supplied this way.
    /// Reserved headers (host, content-type, content-length,
    /// mcp-session-id, mcp-protocol-version, mcp-method, mcp-name,
    /// connection, transfer-encoding) are managed by MCPTracer and cannot be
    /// set here. For modern tools/call requests, Mcp-Param-* values are
    /// derived only from the captured tool input schema and recorded arguments;
    /// manually supplied Mcp-Param-* values are ignored for those calls.
    #[arg(long = "header", value_parser = parse_header)]
    pub headers: Vec<(String, String)>,

    #[arg(long, default_value = "unknown")]
    pub client: String,

    /// Redaction policy applied to the NEW session's stored payloads. Sent
    /// bytes are never modified.
    #[arg(long, default_value = "none")]
    pub redact: String,

    /// Extra JSON field names to mask, comma-separated. Requires
    /// `--redact default` and is stored as the `custom` policy.
    #[arg(long, value_delimiter = ',')]
    pub redact_keys: Vec<String>,

    /// Milliseconds to wait for one request response or a subscriptions/listen
    /// acknowledgment and its remaining captured notification evidence.
    #[arg(long, default_value_t = 30_000)]
    pub request_timeout: u64,

    /// Acknowledge that replay re-executes every recorded request for real
    /// against the live target, and that a source session recorded with
    /// redaction sends the literal `***REDACTED***` placeholder as the
    /// argument value rather than the real secret. Suppresses both warnings
    /// printed before every run.
    #[arg(long)]
    pub i_understand_side_effects: bool,

    /// Only send `tools/call` requests for these tool names (repeatable or
    /// comma-separated). Non-tool protocol messages (`initialize`,
    /// notifications) are never filtered.
    #[arg(long, value_delimiter = ',')]
    pub allow_tool: Vec<String>,

    /// Never send `tools/call` requests for these tool names (repeatable or
    /// comma-separated). Takes precedence over `--allow-tool`.
    #[arg(long, value_delimiter = ',')]
    pub deny_tool: Vec<String>,
}

#[derive(Debug)]
struct HeaderParseError(String);

impl std::fmt::Display for HeaderParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for HeaderParseError {}

fn parse_header(raw: &str) -> Result<(String, String), HeaderParseError> {
    let (name, value) = raw
        .split_once(':')
        .ok_or_else(|| HeaderParseError("must be in the form 'Name: value'".to_string()))?;
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() {
        return Err(HeaderParseError("header name cannot be empty".to_string()));
    }
    let normalized = name.to_ascii_lowercase();
    if RESERVED_HEADERS.contains(&normalized.as_str()) {
        return Err(HeaderParseError(format!(
            "--header cannot set the reserved header '{name}'; mcptracer manages it automatically"
        )));
    }
    Ok((name.to_string(), value.to_string()))
}

pub async fn run(args: ReplayHttpArgs, db_path: PathBuf) -> Result<()> {
    let target = parse_target(&args.target)?;
    let mut extra_headers = HeaderMap::new();
    for (name, value) in &args.headers {
        let header_name = HeaderName::try_from(name.as_str())
            .with_context(|| format!("invalid --header name: {name}"))?;
        let header_value = HeaderValue::try_from(value.as_str())
            .with_context(|| format!("invalid --header value for {name}"))?;
        extra_headers.append(header_name, header_value);
    }

    let store = Store::open(&db_path)?;
    let health = require_healthy_session(&store, &args.session_id, "http replay")?;
    let source_messages = health.messages;
    if source_messages.is_empty() {
        return Err(anyhow!(
            "source session has no recorded messages: {}",
            health.session_id
        ));
    }

    let driven: Vec<&StoredMessage> = driven_messages(&source_messages)
        .into_iter()
        .filter(|m| tool_is_allowed(m.tool_name.as_deref(), &args.allow_tool, &args.deny_tool))
        .collect();
    if driven.is_empty() {
        return Err(anyhow!(
            "source session has no client-originated requests or notifications to replay (after any --allow-tool/--deny-tool filtering)"
        ));
    }
    warn_side_effects(&driven, args.i_understand_side_effects);
    warn_redacted_replay(&driven, args.i_understand_side_effects);

    let policy = RedactionPolicy::from_cli(&args.redact, &args.redact_keys)
        .map_err(|error| anyhow!(error))?;
    let redactor = Redactor::new(policy.clone());

    let client = reqwest::Client::builder()
        .redirect(RedirectPolicy::none())
        .build()
        .context("failed to create Streamable HTTP client")?;

    let target_session_id = store.create_session(
        &args.client,
        &sanitized_target(&target),
        StreamableHttpTransport.name(),
        now_ns(),
    )?;
    if redactor.is_active() {
        store.set_redaction_policy(&target_session_id, policy.as_str(), policy.custom_keys())?;
    }

    let (storage_tx, storage_rx) = mpsc::sync_channel::<StorageEvent>(4096);
    let queue_budget = Arc::new(StorageQueueBudget::new(STORAGE_QUEUE_MAX_BYTES));
    let storage_handle = spawn_storage_writer(
        store,
        target_session_id.clone(),
        redactor,
        storage_rx,
        Arc::clone(&queue_budget),
    );

    eprintln!(
        "[mcptracer] HTTP-replaying session {} as {} -> {}",
        health.session_id,
        target_session_id,
        db_path.display()
    );

    let seq = Arc::new(AtomicU64::new(0));
    let dropped_messages = Arc::new(AtomicU64::new(0));
    let request_timeout = Duration::from_millis(args.request_timeout);
    let mut established_session_id: Option<String> = None;

    let outcome = drive(
        &client,
        &target,
        &extra_headers,
        &driven,
        &source_messages,
        &seq,
        &storage_tx,
        &dropped_messages,
        &queue_budget,
        request_timeout,
        &mut established_session_id,
    )
    .await;

    if let Some(session_header) = &established_session_id {
        terminate_target_session(&client, &target, &extra_headers, session_header).await;
    }

    let storage_result = finish_storage_writer(
        storage_tx,
        storage_handle,
        now_ns(),
        dropped_messages.load(Ordering::Relaxed),
    );

    match (outcome, storage_result) {
        (Ok(()), Ok(())) => {
            eprintln!(
                "[mcptracer] HTTP replay session {} ended",
                target_session_id
            );
            Ok(())
        }
        (Err(err), _) | (_, Err(err)) => {
            eprintln!(
                "[mcptracer] HTTP replay session {} aborted: {}",
                target_session_id, err
            );
            Err(err)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    client: &reqwest::Client,
    target: &Url,
    extra_headers: &HeaderMap,
    driven: &[&StoredMessage],
    source_messages: &[StoredMessage],
    seq: &Arc<AtomicU64>,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &Arc<AtomicU64>,
    queue_budget: &Arc<StorageQueueBudget>,
    request_timeout: Duration,
    established_session_id: &mut Option<String>,
) -> Result<()> {
    let mut driven_index = 0;
    let replay_retry_id = AtomicU64::new(1);
    let mut subscriptions = Vec::new();

    while let Some(first_message) = driven.get(driven_index) {
        let mut source_request: Value =
            serde_json::from_str(&first_message.payload).with_context(|| {
                format!("source message seq {} is not valid JSON", first_message.seq)
            })?;

        if is_modern_subscription_listen(&source_request) {
            let expectation =
                subscription_expectation(source_messages, first_message, &source_request)?;
            if subscriptions
                .iter()
                .any(|subscription: &ActiveSubscription| {
                    subscription.expectation.subscription_id == expectation.subscription_id
                })
            {
                return Err(anyhow!(
                    "source seq {} reuses an active subscriptions/listen JSON-RPC id",
                    first_message.seq
                ));
            }
            subscriptions.push(
                start_subscription_replay(
                    client,
                    target,
                    extra_headers,
                    &source_request,
                    first_message.seq,
                    expectation,
                    seq,
                    storage_tx,
                    dropped_messages,
                    queue_budget,
                    request_timeout,
                )
                .await?,
            );
            driven_index += 1;
            check_active_subscriptions(&mut subscriptions).await?;
            continue;
        }

        let mut replay_request = source_request.clone();
        let mut source_message = *first_message;
        let mut rounds = 0;

        loop {
            let tool_headers = match modern_tool_name(&replay_request)? {
                Some(tool_name) => {
                    tool_header_bindings_at(source_messages, source_message.seq, tool_name)?
                }
                None => Vec::new(),
            };
            let headers = build_request_headers(
                extra_headers,
                established_session_id.as_deref(),
                &replay_request,
                &tool_headers,
            )?;
            let body = serde_json::to_vec(&replay_request)?;
            let is_request = replay_request.get("id").is_some();
            let rpc_id = replay_request.get("id").map(Value::to_string);

            let sent = McpMessage {
                seq: seq.fetch_add(1, Ordering::Relaxed),
                timestamp_ns: now_ns(),
                direction: Direction::ClientToServer,
                payload_bytes: body.len(),
                payload: replay_request.clone(),
            };
            try_record(storage_tx, sent, dropped_messages, queue_budget);

            let send_and_read = async {
                let response = client
                    .post(target.clone())
                    .headers(headers)
                    .body(body)
                    .send()
                    .await
                    .context("HTTP replay request failed")?;
                handle_response(
                    response,
                    is_request,
                    rpc_id.as_deref(),
                    seq,
                    storage_tx,
                    dropped_messages,
                    queue_budget,
                    established_session_id,
                )
                .await
            };

            let input_required = match tokio::time::timeout(request_timeout, send_and_read).await {
                Ok(Ok(input_required)) => input_required,
                Ok(Err(err)) => return Err(err),
                Err(_) => {
                    tracing::warn!(
                        "replayed message (id={:?}) did not complete within {}ms",
                        rpc_id,
                        request_timeout.as_millis()
                    );
                    None
                }
            };
            let Some(input_required) = input_required else {
                break;
            };

            if !is_mrtr_capable_request(&replay_request) {
                return Err(anyhow!(
                    "target returned resultType=input_required for source seq {}; only modern tools/call, prompts/get, and resources/read support MRTR replay",
                    source_message.seq
                ));
            }
            rounds += 1;
            if rounds > MAX_MRTR_ROUNDS {
                return Err(anyhow!(
                    "target requested more than {MAX_MRTR_ROUNDS} multi-round-trip retries for source seq {}",
                    first_message.seq
                ));
            }

            let retry_index = driven_index + 1;
            let retry_message = driven.get(retry_index).ok_or_else(|| {
                anyhow!(
                    "target returned resultType=input_required for source seq {}, but the recording has no following retry request",
                    source_message.seq
                )
            })?;
            let recorded_retry: Value =
                serde_json::from_str(&retry_message.payload).with_context(|| {
                    format!("source message seq {} is not valid JSON", retry_message.seq)
                })?;
            replay_request = build_mrtr_retry_request(
                &source_request,
                &recorded_retry,
                &input_required,
                Value::String(format!(
                    "mcptracer-replay-mrtr-{}",
                    replay_retry_id.fetch_add(1, Ordering::Relaxed)
                )),
            )?;
            source_request = recorded_retry;
            source_message = retry_message;
            driven_index = retry_index;
        }

        driven_index += 1;
        check_active_subscriptions(&mut subscriptions).await?;
    }

    wait_for_subscriptions(&mut subscriptions, request_timeout).await
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct SubscriptionFilter {
    tools_list_changed: bool,
    prompts_list_changed: bool,
    resources_list_changed: bool,
    resource_subscriptions: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct SubscriptionExpectation {
    subscription_id: Value,
    filter: SubscriptionFilter,
    accepted_filter: SubscriptionFilter,
    expected_frames: usize,
}

struct ActiveSubscription {
    source_seq: u64,
    expectation: SubscriptionExpectation,
    received_frames: Arc<AtomicUsize>,
    task: JoinHandle<Result<()>>,
    closed: bool,
}

impl Drop for ActiveSubscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn is_modern_subscription_listen(request: &Value) -> bool {
    modern_protocol_version(request).is_some()
        && request.get("method").and_then(Value::as_str) == Some("subscriptions/listen")
}

fn subscription_expectation(
    source_messages: &[StoredMessage],
    listen: &StoredMessage,
    request: &Value,
) -> Result<SubscriptionExpectation> {
    let subscription_id = request_id(request, "subscriptions/listen source request")?;
    let filter = subscription_filter_from_request(request, "subscriptions/listen source request")?;
    let mut accepted_filter = None;
    let mut expected_frames = 0;

    for message in source_messages {
        if message.direction != "s2c" {
            continue;
        }
        let payload: Value = serde_json::from_str(&message.payload)
            .with_context(|| format!("source message seq {} is not valid JSON", message.seq))?;
        if subscription_id_from_notification(&payload) != Some(&subscription_id) {
            continue;
        }
        if message.seq <= listen.seq {
            return Err(anyhow!(
                "source subscriptions/listen notification seq {} precedes its request seq {}",
                message.seq,
                listen.seq
            ));
        }
        if expected_frames == 0 {
            accepted_filter = Some(
                subscription_acknowledgment_filter(&payload, &subscription_id, &filter)
                    .with_context(|| {
                        format!("invalid source subscription frame at seq {}", message.seq)
                    })?,
            );
        } else {
            validate_subscription_notification(
                &payload,
                &subscription_id,
                accepted_filter
                    .as_ref()
                    .expect("a non-first subscription frame follows an acknowledgment"),
                expected_frames,
            )
            .with_context(|| format!("invalid source subscription frame at seq {}", message.seq))?;
        }
        expected_frames += 1;
    }

    if expected_frames == 0 {
        return Err(anyhow!(
            "source subscriptions/listen request at seq {} has no recorded acknowledgment frame",
            listen.seq
        ));
    }
    let accepted_filter =
        accepted_filter.expect("a nonempty subscription trace starts with acknowledgment");

    Ok(SubscriptionExpectation {
        subscription_id,
        filter,
        accepted_filter,
        expected_frames,
    })
}

#[allow(clippy::too_many_arguments)]
async fn start_subscription_replay(
    client: &reqwest::Client,
    target: &Url,
    extra_headers: &HeaderMap,
    request: &Value,
    source_seq: u64,
    expectation: SubscriptionExpectation,
    seq: &Arc<AtomicU64>,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &Arc<AtomicU64>,
    queue_budget: &Arc<StorageQueueBudget>,
    request_timeout: Duration,
) -> Result<ActiveSubscription> {
    let headers = build_request_headers(extra_headers, None, request, &[])?;
    let body = serde_json::to_vec(request)?;
    let sent = McpMessage {
        seq: seq.fetch_add(1, Ordering::Relaxed),
        timestamp_ns: now_ns(),
        direction: Direction::ClientToServer,
        payload_bytes: body.len(),
        payload: request.clone(),
    };
    try_record(storage_tx, sent, dropped_messages, queue_budget);

    let mut response = match tokio::time::timeout(
        request_timeout,
        client.post(target.clone()).headers(headers).body(body).send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(error).context("subscriptions/listen replay request failed"),
        Err(_) => {
            return Err(anyhow!(
                "subscriptions/listen source seq {source_seq} did not receive response headers within {}ms",
                request_timeout.as_millis()
            ))
        }
    };

    let status = response.status();
    if !status.is_success() {
        let (body_bytes, _) = read_capped(&mut response, MAX_FRAME_BYTES)
            .await
            .unwrap_or_default();
        return Err(anyhow!(
            "target rejected subscriptions/listen source seq {source_seq} with status {status}: {}",
            truncate_for_error(&String::from_utf8_lossy(&body_bytes))
        ));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.starts_with("text/event-stream") {
        return Err(anyhow!(
            "target returned '{}' for subscriptions/listen source seq {source_seq}; modern subscriptions require text/event-stream",
            if content_type.is_empty() { "<none>" } else { &content_type }
        ));
    }

    let received_frames = Arc::new(AtomicUsize::new(0));
    let (ack_tx, ack_rx) = oneshot::channel();
    let task = tokio::spawn(capture_subscription_response(
        response,
        expectation.clone(),
        Arc::clone(seq),
        storage_tx.clone(),
        Arc::clone(dropped_messages),
        Arc::clone(queue_budget),
        Arc::clone(&received_frames),
        ack_tx,
    ));

    match tokio::time::timeout(request_timeout, ack_rx).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(detail))) => {
            task.abort();
            return Err(anyhow!(
                "target subscriptions/listen source seq {source_seq} has invalid acknowledgment: {detail}"
            ));
        }
        Ok(Err(_)) => {
            task.abort();
            return Err(anyhow!(
                "target subscriptions/listen source seq {source_seq} closed before its acknowledgment"
            ));
        }
        Err(_) => {
            task.abort();
            return Err(anyhow!(
                "target subscriptions/listen source seq {source_seq} did not acknowledge within {}ms",
                request_timeout.as_millis()
            ));
        }
    }

    Ok(ActiveSubscription {
        source_seq,
        expectation,
        received_frames,
        task,
        closed: false,
    })
}

#[allow(clippy::too_many_arguments)]
async fn capture_subscription_response(
    mut response: reqwest::Response,
    expectation: SubscriptionExpectation,
    seq: Arc<AtomicU64>,
    storage_tx: SyncSender<StorageEvent>,
    dropped_messages: Arc<AtomicU64>,
    queue_budget: Arc<StorageQueueBudget>,
    received_frames: Arc<AtomicUsize>,
    acknowledgment: oneshot::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let mut decoder = SseDecoder::default();
    let mut acknowledgment = Some(acknowledgment);

    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read subscriptions/listen response stream")?
    {
        for event in decoder.push(&chunk) {
            if let Err(error) = capture_subscription_event(
                event,
                &expectation,
                &seq,
                &storage_tx,
                &dropped_messages,
                &queue_budget,
                &received_frames,
                &mut acknowledgment,
            ) {
                signal_subscription_acknowledgment(&mut acknowledgment, Err(error.to_string()));
                return Err(error);
            }
        }
    }
    for event in decoder.finish() {
        if let Err(error) = capture_subscription_event(
            event,
            &expectation,
            &seq,
            &storage_tx,
            &dropped_messages,
            &queue_budget,
            &received_frames,
            &mut acknowledgment,
        ) {
            signal_subscription_acknowledgment(&mut acknowledgment, Err(error.to_string()));
            return Err(error);
        }
    }

    if acknowledgment.is_some() {
        signal_subscription_acknowledgment(
            &mut acknowledgment,
            Err("stream ended before notifications/subscriptions/acknowledged".to_string()),
        );
        return Err(anyhow!(
            "subscriptions/listen stream ended before notifications/subscriptions/acknowledged"
        ));
    }
    let received = received_frames.load(Ordering::Relaxed);
    if received != expectation.expected_frames {
        return Err(anyhow!(
            "subscriptions/listen stream ended after {received} captured frame(s); source requires {}",
            expectation.expected_frames
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn capture_subscription_event(
    event: SseEvent,
    expectation: &SubscriptionExpectation,
    seq: &Arc<AtomicU64>,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &Arc<AtomicU64>,
    queue_budget: &Arc<StorageQueueBudget>,
    received_frames: &Arc<AtomicUsize>,
    acknowledgment: &mut Option<oneshot::Sender<std::result::Result<(), String>>>,
) -> Result<()> {
    let bytes = match event {
        SseEvent::Json(bytes) => bytes,
        SseEvent::Oversized => {
            return Err(anyhow!(
                "subscriptions/listen stream contains an oversized SSE JSON frame"
            ))
        }
    };
    let payload: Value = serde_json::from_slice(&bytes)
        .context("subscriptions/listen stream contains non-JSON frame")?;
    if is_subscription_terminal_response(&payload, &expectation.subscription_id) {
        record_bytes(&bytes, seq, storage_tx, dropped_messages, queue_budget);
        return Ok(());
    }

    let frame_index = received_frames.load(Ordering::Relaxed);
    if frame_index >= expectation.expected_frames {
        return Err(anyhow!(
            "target subscriptions/listen stream produced more frames than the source recording"
        ));
    }
    if frame_index == 0 {
        validate_subscription_acknowledgment(
            &payload,
            &expectation.subscription_id,
            &expectation.filter,
            &expectation.accepted_filter,
        )?;
    } else {
        validate_subscription_notification(
            &payload,
            &expectation.subscription_id,
            &expectation.accepted_filter,
            frame_index,
        )?;
    }
    record_bytes(&bytes, seq, storage_tx, dropped_messages, queue_budget);
    received_frames.fetch_add(1, Ordering::Relaxed);
    if frame_index == 0 {
        signal_subscription_acknowledgment(acknowledgment, Ok(()));
    }
    Ok(())
}

fn signal_subscription_acknowledgment(
    acknowledgment: &mut Option<oneshot::Sender<std::result::Result<(), String>>>,
    result: std::result::Result<(), String>,
) {
    if let Some(sender) = acknowledgment.take() {
        let _ = sender.send(result);
    }
}

async fn check_active_subscriptions(subscriptions: &mut [ActiveSubscription]) -> Result<()> {
    for subscription in subscriptions {
        if subscription.closed || !subscription.task.is_finished() {
            continue;
        }
        let result = (&mut subscription.task)
            .await
            .map_err(|error| anyhow!("subscriptions/listen worker failed: {error}"))?;
        subscription.closed = true;
        result?;
        let received = subscription.received_frames.load(Ordering::Relaxed);
        if received != subscription.expectation.expected_frames {
            return Err(anyhow!(
                "subscriptions/listen source seq {} ended after {received} captured frame(s); source requires {}",
                subscription.source_seq,
                subscription.expectation.expected_frames
            ));
        }
    }
    Ok(())
}

async fn wait_for_subscriptions(
    subscriptions: &mut [ActiveSubscription],
    request_timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + request_timeout;
    loop {
        check_active_subscriptions(subscriptions).await?;
        if subscriptions.iter().all(|subscription| {
            subscription.received_frames.load(Ordering::Relaxed)
                == subscription.expectation.expected_frames
        }) {
            break;
        }
        if Instant::now() >= deadline {
            let pending = subscriptions
                .iter()
                .filter(|subscription| {
                    subscription.received_frames.load(Ordering::Relaxed)
                        != subscription.expectation.expected_frames
                })
                .map(|subscription| subscription.source_seq.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(anyhow!(
                "timed out after {}ms waiting for subscriptions/listen evidence from source seq {pending}",
                request_timeout.as_millis()
            ));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    for subscription in subscriptions {
        if !subscription.closed {
            subscription.task.abort();
            let _ = (&mut subscription.task).await;
            subscription.closed = true;
        }
    }
    Ok(())
}

fn request_id(request: &Value, context: &str) -> Result<Value> {
    match request.get("id") {
        Some(Value::String(id)) => Ok(Value::String(id.clone())),
        Some(Value::Number(id)) => Ok(Value::Number(id.clone())),
        _ => Err(anyhow!(
            "{context} must carry a string or numeric JSON-RPC id"
        )),
    }
}

fn subscription_filter_from_request(request: &Value, context: &str) -> Result<SubscriptionFilter> {
    let notifications = request
        .pointer("/params/notifications")
        .ok_or_else(|| anyhow!("{context} is missing params.notifications"))?;
    subscription_filter_from_value(notifications, context)
}

fn subscription_filter_from_value(value: &Value, context: &str) -> Result<SubscriptionFilter> {
    let notifications = value
        .as_object()
        .ok_or_else(|| anyhow!("{context} notifications must be an object"))?;
    let mut filter = SubscriptionFilter {
        tools_list_changed: false,
        prompts_list_changed: false,
        resources_list_changed: false,
        resource_subscriptions: BTreeSet::new(),
    };
    for (name, setting) in notifications {
        match name.as_str() {
            "toolsListChanged" => {
                filter.tools_list_changed = setting.as_bool().ok_or_else(|| {
                    anyhow!("{context} notifications.toolsListChanged must be boolean")
                })?;
            }
            "promptsListChanged" => {
                filter.prompts_list_changed = setting.as_bool().ok_or_else(|| {
                    anyhow!("{context} notifications.promptsListChanged must be boolean")
                })?;
            }
            "resourcesListChanged" => {
                filter.resources_list_changed = setting.as_bool().ok_or_else(|| {
                    anyhow!("{context} notifications.resourcesListChanged must be boolean")
                })?;
            }
            "resourceSubscriptions" => {
                let uris = setting.as_array().ok_or_else(|| {
                    anyhow!("{context} notifications.resourceSubscriptions must be an array")
                })?;
                for uri in uris {
                    let uri = uri.as_str().ok_or_else(|| {
                        anyhow!(
                            "{context} notifications.resourceSubscriptions entries must be strings"
                        )
                    })?;
                    if !filter.resource_subscriptions.insert(uri.to_string()) {
                        return Err(anyhow!(
                            "{context} notifications.resourceSubscriptions contains duplicate '{uri}'"
                        ));
                    }
                }
            }
            _ => {
                return Err(anyhow!(
                    "{context} requests unsupported notification filter '{name}'"
                ))
            }
        }
    }
    Ok(filter)
}

fn subscription_id_from_notification(payload: &Value) -> Option<&Value> {
    payload.pointer("/params/_meta/io.modelcontextprotocol~1subscriptionId")
}

fn subscription_acknowledgment_filter(
    payload: &Value,
    subscription_id: &Value,
    requested_filter: &SubscriptionFilter,
) -> Result<SubscriptionFilter> {
    validate_subscription_notification(payload, subscription_id, requested_filter, 0)?;
    let accepted = payload
        .pointer("/params/notifications")
        .expect("validated subscription acknowledgment has params.notifications");
    subscription_filter_from_value(accepted, "subscription acknowledgment")
}

fn validate_subscription_acknowledgment(
    payload: &Value,
    subscription_id: &Value,
    requested_filter: &SubscriptionFilter,
    expected_filter: &SubscriptionFilter,
) -> Result<()> {
    let accepted = subscription_acknowledgment_filter(payload, subscription_id, requested_filter)?;
    if &accepted != expected_filter {
        return Err(anyhow!(
            "subscription acknowledgment accepted a different filter than the source recording"
        ));
    }
    Ok(())
}

fn validate_subscription_notification(
    payload: &Value,
    subscription_id: &Value,
    filter: &SubscriptionFilter,
    frame_index: usize,
) -> Result<()> {
    if payload.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(anyhow!("subscription frame is missing jsonrpc=2.0"));
    }
    if payload.get("id").is_some() {
        return Err(anyhow!(
            "subscription stream must not contain JSON-RPC requests"
        ));
    }
    if subscription_id_from_notification(payload) != Some(subscription_id) {
        return Err(anyhow!(
            "subscription frame is missing the originating io.modelcontextprotocol/subscriptionId"
        ));
    }
    let method = payload
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("subscription frame is missing a string notification method"))?;

    if frame_index == 0 {
        if method != "notifications/subscriptions/acknowledged" {
            return Err(anyhow!(
                "first subscription frame must be notifications/subscriptions/acknowledged, found '{method}'"
            ));
        }
        let accepted = payload.pointer("/params/notifications").ok_or_else(|| {
            anyhow!("subscription acknowledgment is missing params.notifications")
        })?;
        let accepted = subscription_filter_from_value(accepted, "subscription acknowledgment")?;
        if (accepted.tools_list_changed && !filter.tools_list_changed)
            || (accepted.prompts_list_changed && !filter.prompts_list_changed)
            || (accepted.resources_list_changed && !filter.resources_list_changed)
            || !accepted
                .resource_subscriptions
                .is_subset(&filter.resource_subscriptions)
        {
            return Err(anyhow!(
                "subscription acknowledgment accepts notifications outside the requested filter"
            ));
        }
        return Ok(());
    }

    match method {
        "notifications/tools/list_changed" if filter.tools_list_changed => Ok(()),
        "notifications/prompts/list_changed" if filter.prompts_list_changed => Ok(()),
        "notifications/resources/list_changed" if filter.resources_list_changed => Ok(()),
        "notifications/resources/updated" if !filter.resource_subscriptions.is_empty() => {
            let uri = payload.pointer("/params/uri").and_then(Value::as_str);
            match uri {
                Some(uri) if filter.resource_subscriptions.contains(uri) => Ok(()),
                Some(uri) => Err(anyhow!(
                    "notifications/resources/updated uri '{uri}' is outside the requested filter"
                )),
                None => Err(anyhow!(
                    "notifications/resources/updated must include a non-empty params.uri"
                )),
            }
        }
        _ => Err(anyhow!(
            "subscription stream emitted '{method}', which is not in the requested filter"
        )),
    }
}

fn is_subscription_terminal_response(payload: &Value, subscription_id: &Value) -> bool {
    payload.get("id") == Some(subscription_id)
        && (payload.get("result").is_some() || payload.get("error").is_some())
}
fn build_request_headers(
    extra: &HeaderMap,
    session_id: Option<&str>,
    request: &Value,
    tool_headers: &[ToolHeaderBinding],
) -> Result<HeaderMap> {
    let modern_tool = modern_tool_name(request)?.is_some();
    let mut headers = HeaderMap::new();
    for (name, value) in extra {
        if !modern_tool || !name.as_str().starts_with("mcp-param-") {
            headers.append(name.clone(), value.clone());
        }
    }
    if modern_tool {
        insert_tool_headers(&mut headers, request, tool_headers)?;
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    if let Some(protocol_version) = modern_protocol_version(request) {
        insert_header(&mut headers, "mcp-protocol-version", protocol_version)?;
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("modern MCP request is missing a string method"))?;
        insert_header(&mut headers, "mcp-method", method)?;
        if let Some(name_field) = required_name_field(method) {
            let name = request
                .get("params")
                .and_then(|params| params.get(name_field))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    anyhow!(
                        "modern MCP {method} request is missing string params.{name_field} required for Mcp-Name"
                    )
                })?;
            insert_header(&mut headers, "mcp-name", &encode_mcp_header_value(name))?;
        }
    } else if let Some(session_id) = session_id {
        if let Ok(value) = HeaderValue::from_str(session_id) {
            headers.insert(HeaderName::from_static("mcp-session-id"), value);
        }
    }
    Ok(headers)
}

fn modern_protocol_version(request: &Value) -> Option<&str> {
    let key = "io.modelcontextprotocol/protocolVersion";
    request
        .get("params")
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get(key))
        .or_else(|| request.get("_meta").and_then(|meta| meta.get(key)))
        .and_then(Value::as_str)
}

fn required_name_field(method: &str) -> Option<&'static str> {
    match method {
        "tools/call" | "prompts/get" => Some("name"),
        "resources/read" => Some("uri"),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolParamType {
    String,
    Integer,
    Boolean,
}

#[derive(Debug, Eq, PartialEq)]
struct ToolHeaderBinding {
    header_name: HeaderName,
    path: Vec<String>,
    value_type: ToolParamType,
}

fn modern_tool_name(request: &Value) -> Result<Option<&str>> {
    if modern_protocol_version(request).is_none()
        || request.get("method").and_then(Value::as_str) != Some("tools/call")
    {
        return Ok(None);
    }
    request
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .map(Some)
        .ok_or_else(|| anyhow!("modern MCP tools/call request is missing string params.name"))
}

fn tool_header_bindings_at(
    messages: &[StoredMessage],
    call_seq: u64,
    tool_name: &str,
) -> Result<Vec<ToolHeaderBinding>> {
    for (index, response) in messages.iter().enumerate().rev() {
        if response.seq >= call_seq
            || response.direction != "s2c"
            || !is_tools_list_response(messages, index)
        {
            continue;
        }
        let payload: Value = serde_json::from_str(&response.payload).with_context(|| {
            format!(
                "captured tools/list response seq {} is not valid JSON",
                response.seq
            )
        })?;
        let Some(tools) = payload
            .get("result")
            .and_then(|result| result.get("tools"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        let Some(tool) = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(tool_name))
        else {
            return Err(anyhow!(
                "cannot replay modern tools/call '{tool_name}' at source seq {call_seq}: the latest captured tools/list response (seq {}) does not define it",
                response.seq
            ));
        };
        return tool
            .get("inputSchema")
            .map(collect_tool_header_bindings)
            .transpose()
            .map(|bindings| bindings.unwrap_or_default())
            .with_context(|| format!(
                "captured inputSchema for modern tool '{tool_name}' in tools/list response seq {} is invalid",
                response.seq
            ));
    }
    Err(anyhow!(
        "cannot replay modern tools/call '{tool_name}' at source seq {call_seq}: no prior captured tools/list definition is available to derive Mcp-Param-* headers"
    ))
}

fn is_tools_list_response(messages: &[StoredMessage], response_index: usize) -> bool {
    let response = &messages[response_index];
    let Some(rpc_id) = response.rpc_id.as_deref() else {
        return false;
    };
    messages[..response_index]
        .iter()
        .rev()
        .find(|request| request.direction == "c2s" && request.rpc_id.as_deref() == Some(rpc_id))
        .is_some_and(|request| {
            request.message_kind == "request" && request.method.as_deref() == Some("tools/list")
        })
}

fn collect_tool_header_bindings(schema: &Value) -> Result<Vec<ToolHeaderBinding>> {
    let schema = schema
        .as_object()
        .ok_or_else(|| anyhow!("inputSchema must be a JSON object"))?;
    let mut bindings = Vec::new();
    let mut seen_headers = HashSet::new();
    visit_schema(schema, &[], true, false, &mut bindings, &mut seen_headers)?;
    Ok(bindings)
}

fn visit_schema(
    schema: &serde_json::Map<String, Value>,
    path: &[String],
    properties_only: bool,
    is_property: bool,
    bindings: &mut Vec<ToolHeaderBinding>,
    seen_headers: &mut HashSet<String>,
) -> Result<()> {
    if let Some(annotation) = schema.get("x-mcp-header") {
        if !properties_only || !is_property {
            return Err(anyhow!(
                "x-mcp-header is allowed only on a primitive property reached through inputSchema.properties"
            ));
        }
        let annotation = annotation
            .as_str()
            .filter(|value| !value.is_empty() && is_http_token(value))
            .ok_or_else(|| anyhow!("x-mcp-header must be a non-empty HTTP token string"))?;
        let value_type = match schema.get("type").and_then(Value::as_str) {
            Some("string") => ToolParamType::String,
            Some("integer") => ToolParamType::Integer,
            Some("boolean") => ToolParamType::Boolean,
            Some(other) => return Err(anyhow!(
                "x-mcp-header property '{}' has unsupported type '{other}'; expected string, integer, or boolean",
                display_schema_path(path)
            )),
            None => return Err(anyhow!(
                "x-mcp-header property '{}' must declare type string, integer, or boolean",
                display_schema_path(path)
            )),
        };
        let header_name =
            HeaderName::try_from(format!("mcp-param-{annotation}")).with_context(|| {
                format!("x-mcp-header '{annotation}' does not form a valid Mcp-Param header name")
            })?;
        if !seen_headers.insert(header_name.as_str().to_ascii_lowercase()) {
            return Err(anyhow!(
                "x-mcp-header '{annotation}' is duplicated case-insensitively in one inputSchema"
            ));
        }
        bindings.push(ToolHeaderBinding {
            header_name,
            path: path.to_vec(),
            value_type,
        });
    }

    for (keyword, value) in schema {
        if keyword == "x-mcp-header" {
            continue;
        }
        if keyword == "properties" {
            if let Some(properties) = value.as_object() {
                for (name, child) in properties {
                    let mut child_path = path.to_vec();
                    child_path.push(name.clone());
                    visit_value(
                        child,
                        &child_path,
                        properties_only,
                        true,
                        bindings,
                        seen_headers,
                    )?;
                }
            } else if contains_header_annotation(value) {
                return Err(anyhow!("inputSchema.properties must be an object"));
            }
        } else if contains_header_annotation(value) {
            visit_value(value, path, false, false, bindings, seen_headers)?;
        }
    }
    Ok(())
}

fn visit_value(
    value: &Value,
    path: &[String],
    properties_only: bool,
    is_property: bool,
    bindings: &mut Vec<ToolHeaderBinding>,
    seen_headers: &mut HashSet<String>,
) -> Result<()> {
    match value {
        Value::Object(schema) => visit_schema(
            schema,
            path,
            properties_only,
            is_property,
            bindings,
            seen_headers,
        ),
        Value::Array(values) => {
            for value in values {
                visit_value(value, path, false, false, bindings, seen_headers)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn contains_header_annotation(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key("x-mcp-header") || object.values().any(contains_header_annotation)
        }
        Value::Array(values) => values.iter().any(contains_header_annotation),
        _ => false,
    }
}

fn is_http_token(value: &str) -> bool {
    value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | 0x27
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | 0x60
                    | b'|'
                    | b'~'
            )
    })
}

fn insert_tool_headers(
    headers: &mut HeaderMap,
    request: &Value,
    bindings: &[ToolHeaderBinding],
) -> Result<()> {
    let arguments = request
        .get("params")
        .and_then(|params| params.get("arguments"));
    for binding in bindings {
        let Some(value) =
            arguments.and_then(|arguments| value_at_property_path(arguments, &binding.path))
        else {
            continue;
        };
        let encoded = encode_tool_header_value(value, binding)?;
        let value = HeaderValue::from_str(&encoded).with_context(|| {
            format!(
                "derived {} value for '{}' is not a valid HTTP header value",
                binding.header_name,
                display_schema_path(&binding.path)
            )
        })?;
        headers.insert(binding.header_name.clone(), value);
    }
    Ok(())
}

fn value_at_property_path<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |value, property| value.get(property))
}

fn encode_tool_header_value(value: &Value, binding: &ToolHeaderBinding) -> Result<String> {
    match binding.value_type {
        ToolParamType::String => value.as_str().map(encode_mcp_header_value).ok_or_else(|| {
            anyhow!(
                "argument '{}' must be a string to populate {}",
                display_schema_path(&binding.path),
                binding.header_name
            )
        }),
        ToolParamType::Boolean => value
            .as_bool()
            .map(|value| value.to_string())
            .ok_or_else(|| {
                anyhow!(
                    "argument '{}' must be a boolean to populate {}",
                    display_schema_path(&binding.path),
                    binding.header_name
                )
            }),
        ToolParamType::Integer => {
            const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
            let integer = value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|value| i64::try_from(value).ok()))
                .filter(|value| value.unsigned_abs() <= MAX_SAFE_INTEGER as u64)
                .ok_or_else(|| {
                    anyhow!(
                        "argument '{}' must be a JavaScript-safe integer to populate {}",
                        display_schema_path(&binding.path),
                        binding.header_name
                    )
                })?;
            Ok(integer.to_string())
        }
    }
}

fn display_schema_path(path: &[String]) -> String {
    if path.is_empty() {
        "inputSchema".to_string()
    } else {
        format!("arguments.{}", path.join("."))
    }
}
const MAX_MRTR_ROUNDS: usize = 10;

#[derive(Debug)]
struct InputRequired {
    input_response_keys: BTreeSet<String>,
    request_state: Option<String>,
}

fn is_mrtr_capable_request(request: &Value) -> bool {
    modern_protocol_version(request).is_some()
        && matches!(
            request.get("method").and_then(Value::as_str),
            Some("tools/call" | "prompts/get" | "resources/read")
        )
}

fn build_mrtr_retry_request(
    source_request: &Value,
    recorded_retry: &Value,
    input_required: &InputRequired,
    replay_id: Value,
) -> Result<Value> {
    let source_method = source_request
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("MRTR source request is missing a string method"))?;
    let retry_method = recorded_retry
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("MRTR recorded retry is missing a string method"))?;
    if source_method != retry_method {
        return Err(anyhow!(
            "MRTR recorded retry method '{retry_method}' does not match original method '{source_method}'"
        ));
    }
    if request_without_mrtr_fields(source_request)? != request_without_mrtr_fields(recorded_retry)?
    {
        return Err(anyhow!(
            "MRTR recorded retry does not preserve the original request outside inputResponses, requestState, and JSON-RPC id"
        ));
    }

    let retry_params = recorded_retry
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("MRTR recorded retry is missing object params"))?;
    let input_response_keys = match retry_params.get("inputResponses") {
        Some(Value::Object(responses)) => responses.keys().cloned().collect(),
        Some(_) => return Err(anyhow!("MRTR recorded retry has non-object inputResponses")),
        None => BTreeSet::new(),
    };
    if input_response_keys != input_required.input_response_keys {
        return Err(anyhow!(
            "MRTR recorded inputResponses keys do not match the target inputRequests keys"
        ));
    }

    let recorded_state = match retry_params.get("requestState") {
        Some(Value::String(state)) => Some(state),
        Some(_) => return Err(anyhow!("MRTR recorded retry has non-string requestState")),
        None => None,
    };
    if recorded_state.is_some() != input_required.request_state.is_some() {
        return Err(anyhow!(
            "MRTR recorded retry requestState presence does not match the target input_required result"
        ));
    }

    let mut retry = recorded_retry.clone();
    let params = retry
        .get_mut("params")
        .and_then(Value::as_object_mut)
        .expect("validated recorded retry params must remain an object");
    match &input_required.request_state {
        Some(request_state) => {
            params.insert(
                "requestState".to_string(),
                Value::String(request_state.clone()),
            );
        }
        None => {
            params.remove("requestState");
        }
    }
    retry
        .as_object_mut()
        .expect("validated recorded retry must remain an object")
        .insert("id".to_string(), replay_id);
    Ok(retry)
}

fn request_without_mrtr_fields(request: &Value) -> Result<Value> {
    let mut base = request.clone();
    let object = base
        .as_object_mut()
        .ok_or_else(|| anyhow!("MRTR recorded request must be a JSON object"))?;
    object.remove("id");
    let params = object
        .get_mut("params")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("MRTR recorded request is missing object params"))?;
    params.remove("inputResponses");
    params.remove("requestState");
    Ok(base)
}

fn input_required_from_bytes(bytes: &[u8]) -> Result<Option<InputRequired>> {
    let Ok(payload) = serde_json::from_slice::<Value>(bytes) else {
        return Ok(None);
    };
    let Some(result) = payload.get("result").and_then(Value::as_object) else {
        return Ok(None);
    };
    if result.get("resultType").and_then(Value::as_str) != Some("input_required") {
        return Ok(None);
    }

    let input_response_keys = match result.get("inputRequests") {
        Some(Value::Object(requests)) => requests.keys().cloned().collect(),
        Some(_) => {
            return Err(anyhow!(
                "target input_required result has non-object inputRequests"
            ))
        }
        None => BTreeSet::new(),
    };
    let request_state = match result.get("requestState") {
        Some(Value::String(state)) => Some(state.clone()),
        Some(_) => {
            return Err(anyhow!(
                "target input_required result has non-string requestState"
            ))
        }
        None => None,
    };
    if !result.contains_key("inputRequests") && request_state.is_none() {
        return Err(anyhow!(
            "target input_required result must contain inputRequests or requestState"
        ));
    }
    Ok(Some(InputRequired {
        input_response_keys,
        request_state,
    }))
}
fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    let value = HeaderValue::from_str(value)
        .with_context(|| format!("modern MCP request has an invalid {name} header value"))?;
    headers.insert(HeaderName::from_static(name), value);
    Ok(())
}

fn encode_mcp_header_value(value: &str) -> String {
    let bytes = value.as_bytes();
    let sentinel = value.starts_with("=?base64?") && value.ends_with("?=");
    let plain = !bytes.is_empty()
        && bytes
            .iter()
            .all(|byte| matches!(*byte, 0x20..=0x7e | b'\t'))
        && !matches!(bytes.first(), Some(b' ' | b'\t'))
        && !matches!(bytes.last(), Some(b' ' | b'\t'))
        && !sentinel;
    if plain {
        value.to_string()
    } else {
        format!("=?base64?{}?=", BASE64_STANDARD.encode(bytes))
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_response(
    mut response: reqwest::Response,
    is_request: bool,
    rpc_id: Option<&str>,
    seq: &AtomicU64,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
    established_session_id: &mut Option<String>,
) -> Result<Option<InputRequired>> {
    let status = response.status();
    if established_session_id.is_none() {
        if let Some(session_header) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *established_session_id = Some(session_header.to_string());
        }
    }

    if !status.is_success() {
        let (body_bytes, _truncated) = read_capped(&mut response, MAX_FRAME_BYTES)
            .await
            .unwrap_or_default();
        let body = String::from_utf8_lossy(&body_bytes);
        return Err(anyhow!(
            "target rejected replayed message with status {status}: {}",
            truncate_for_error(&body)
        ));
    }

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if content_type.starts_with("text/event-stream") {
        capture_sse_response(response, seq, storage_tx, dropped_messages, queue_budget).await
    } else if content_type.starts_with("application/json") {
        capture_json_response(response, seq, storage_tx, dropped_messages, queue_budget).await
    } else if !is_request || status == reqwest::StatusCode::ACCEPTED {
        Ok(None)
    } else {
        Err(anyhow!(
            "server returned an unsupported response envelope '{}' for request id={:?}; MCP Streamable HTTP requires application/json or text/event-stream",
            if content_type.is_empty() { "<none>" } else { &content_type },
            rpc_id
        ))
    }
}

async fn capture_json_response(
    mut response: reqwest::Response,
    seq: &AtomicU64,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
) -> Result<Option<InputRequired>> {
    let (bytes, too_large) = read_capped(&mut response, MAX_FRAME_BYTES)
        .await
        .context("failed to read JSON response body")?;
    if too_large {
        dropped_messages.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("JSON response body exceeded {MAX_FRAME_BYTES} bytes; not recorded");
        return Ok(None);
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    let input_required = input_required_from_bytes(&bytes);
    record_bytes(&bytes, seq, storage_tx, dropped_messages, queue_budget);
    input_required
}

async fn read_capped(response: &mut reqwest::Response, cap: usize) -> Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read response body")?
    {
        if buf.len().saturating_add(chunk.len()) > cap {
            return Ok((buf, true));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((buf, false))
}

async fn capture_sse_response(
    mut response: reqwest::Response,
    seq: &AtomicU64,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
) -> Result<Option<InputRequired>> {
    let mut decoder = SseDecoder::default();
    let mut input_required = None;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read SSE response body")?
    {
        for event in decoder.push(&chunk) {
            collect_input_required(
                &mut input_required,
                record_sse_event(event, seq, storage_tx, dropped_messages, queue_budget)?,
            )?;
        }
    }
    for event in decoder.finish() {
        collect_input_required(
            &mut input_required,
            record_sse_event(event, seq, storage_tx, dropped_messages, queue_budget)?,
        )?;
    }
    Ok(input_required)
}

fn collect_input_required(
    observed: &mut Option<InputRequired>,
    next: Option<InputRequired>,
) -> Result<()> {
    if let Some(next) = next {
        if observed.replace(next).is_some() {
            return Err(anyhow!(
                "target response contains more than one input_required result"
            ));
        }
    }
    Ok(())
}

fn record_sse_event(
    event: SseEvent,
    seq: &AtomicU64,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
) -> Result<Option<InputRequired>> {
    match event {
        SseEvent::Json(bytes) => {
            let input_required = input_required_from_bytes(&bytes);
            record_bytes(&bytes, seq, storage_tx, dropped_messages, queue_budget);
            input_required
        }
        SseEvent::Oversized => {
            dropped_messages.fetch_add(1, Ordering::Relaxed);
            tracing::warn!("SSE data event exceeded the frame size limit; not recorded");
            Ok(None)
        }
    }
}
fn record_bytes(
    bytes: &[u8],
    seq: &AtomicU64,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
) {
    let stamp_seq = seq.fetch_add(1, Ordering::Relaxed);
    match StreamableHttpTransport.decode_message(
        bytes,
        Direction::ServerToClient,
        stamp_seq,
        now_ns(),
    ) {
        Ok(message) => try_record(storage_tx, message, dropped_messages, queue_budget),
        Err(error) => {
            dropped_messages.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                "received non-JSON Streamable HTTP payload from the target but did not record it: {error}"
            );
        }
    }
}

async fn terminate_target_session(
    client: &reqwest::Client,
    target: &Url,
    extra_headers: &HeaderMap,
    session_id: &str,
) {
    let mut headers = extra_headers.clone();
    if let Ok(value) = HeaderValue::from_str(session_id) {
        headers.insert(HeaderName::from_static("mcp-session-id"), value);
    }
    if let Err(error) = client.delete(target.clone()).headers(headers).send().await {
        tracing::warn!("failed to send session-termination DELETE to target: {error}");
    }
}

fn truncate_for_error(body: &str) -> String {
    const MAX_CHARS: usize = 500;
    if body.chars().count() > MAX_CHARS {
        let mut truncated: String = body.chars().take(MAX_CHARS).collect();
        truncated.push_str("... (truncated)");
        truncated
    } else {
        body.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_mrtr_retry_request, build_request_headers, collect_tool_header_bindings,
        encode_mcp_header_value, input_required_from_bytes, parse_header, subscription_expectation,
        tool_header_bindings_at, truncate_for_error, validate_subscription_acknowledgment,
        validate_subscription_notification, InputRequired, SubscriptionFilter, ToolParamType,
    };
    use mcptracer_storage::StoredMessage;
    use reqwest::header::HeaderMap;
    use serde_json::{json, Value};
    use std::collections::BTreeSet;

    fn stored(
        seq: u64,
        direction: &str,
        kind: &str,
        rpc_id: Option<&str>,
        method: Option<&str>,
        payload: serde_json::Value,
    ) -> StoredMessage {
        StoredMessage {
            seq,
            ts_ns: seq as i64,
            direction: direction.to_string(),
            message_kind: kind.to_string(),
            rpc_id: rpc_id.map(str::to_string),
            method: method.map(str::to_string),
            tool_name: None,
            payload: payload.to_string(),
            payload_bytes: 0,
            is_error: false,
            error_code: None,
        }
    }

    fn header_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"},
                "attempt": {"type": "integer", "x-mcp-header": "Attempt"},
                "config": {
                    "type": "object",
                    "properties": {
                        "enabled": {"type": "boolean", "x-mcp-header": "Enabled"}
                    }
                },
                "omitted": {"type": "string", "x-mcp-header": "Omitted"}
            }
        })
    }

    #[test]
    fn schema_derived_headers_encode_primitive_arguments_and_ignore_manual_values() {
        let bindings = collect_tool_header_bindings(&header_schema()).unwrap();
        assert_eq!(bindings.len(), 4);
        assert!(bindings
            .iter()
            .any(|binding| binding.value_type == ToolParamType::String));
        assert!(bindings
            .iter()
            .any(|binding| binding.value_type == ToolParamType::Integer));
        assert!(bindings
            .iter()
            .any(|binding| binding.value_type == ToolParamType::Boolean));

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "echo",
                "arguments": {"region": " India ", "attempt": 2, "config": {"enabled": true}},
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
            }
        });
        let mut extra = HeaderMap::new();
        extra.insert("mcp-param-region", "manual".parse().unwrap());
        extra.insert("x-api-key", "credential".parse().unwrap());

        let headers = build_request_headers(&extra, None, &request, &bindings).unwrap();
        assert_eq!(headers["mcp-param-region"], "=?base64?IEluZGlhIA==?=");
        assert_eq!(headers["mcp-param-attempt"], "2");
        assert_eq!(headers["mcp-param-enabled"], "true");
        assert!(!headers.contains_key("mcp-param-omitted"));
        assert_eq!(headers["x-api-key"], "credential");
    }

    #[test]
    fn schema_header_annotations_fail_closed_when_invalid() {
        assert!(collect_tool_header_bindings(&json!({
            "type": "object",
            "properties": {"ratio": {"type": "number", "x-mcp-header": "Ratio"}}
        }))
        .is_err());
        assert!(collect_tool_header_bindings(&json!({
            "type": "object",
            "properties": {
                "first": {"type": "string", "x-mcp-header": "Region"},
                "second": {"type": "string", "x-mcp-header": "region"}
            }
        }))
        .is_err());
        assert!(collect_tool_header_bindings(&json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {"type": "string", "x-mcp-header": "Item"}
                }
            }
        }))
        .is_err());
    }

    #[test]
    fn resolver_uses_only_prior_tools_list_definition() {
        let list_response = json!({
            "jsonrpc": "2.0",
            "id": "list-1",
            "result": {"tools": [{"name": "echo", "inputSchema": header_schema()}]}
        });
        let messages = vec![
            stored(
                0,
                "c2s",
                "request",
                Some("list-1"),
                Some("tools/list"),
                json!({}),
            ),
            stored(1, "s2c", "response", Some("list-1"), None, list_response),
            stored(
                2,
                "c2s",
                "request",
                Some("call-1"),
                Some("tools/call"),
                json!({}),
            ),
        ];
        assert_eq!(
            tool_header_bindings_at(&messages, 2, "echo").unwrap().len(),
            4
        );

        let future_only = vec![
            stored(
                0,
                "c2s",
                "request",
                Some("call-1"),
                Some("tools/call"),
                json!({}),
            ),
            stored(
                1,
                "c2s",
                "request",
                Some("list-1"),
                Some("tools/list"),
                json!({}),
            ),
            stored(
                2,
                "s2c",
                "response",
                Some("list-1"),
                None,
                json!({
                    "jsonrpc": "2.0",
                    "id": "list-1",
                    "result": {"tools": [{"name": "echo", "inputSchema": header_schema()}]}
                }),
            ),
        ];
        assert!(tool_header_bindings_at(&future_only, 0, "echo").is_err());
    }

    #[test]
    fn integer_header_rejects_values_outside_the_safe_javascript_range() {
        let bindings = collect_tool_header_bindings(&header_schema()).unwrap();
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "echo",
                "arguments": {"attempt": 9007199254740992_u64},
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
            }
        });
        assert!(build_request_headers(&HeaderMap::new(), None, &request, &bindings).is_err());
    }

    #[test]
    fn subscription_evidence_requires_acknowledgment_and_honors_filter() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": "listen-1",
            "method": "subscriptions/listen",
            "params": {
                "notifications": {
                    "toolsListChanged": true,
                    "promptsListChanged": true
                },
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
            }
        });
        let source = vec![
            stored(
                0,
                "c2s",
                "request",
                Some("\"listen-1\""),
                Some("subscriptions/listen"),
                request.clone(),
            ),
            stored(
                1,
                "s2c",
                "notification",
                None,
                Some("notifications/subscriptions/acknowledged"),
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/subscriptions/acknowledged",
                    "params": {
                        "notifications": {"toolsListChanged": true},
                        "_meta": {"io.modelcontextprotocol/subscriptionId": "listen-1"}
                    }
                }),
            ),
            stored(
                2,
                "s2c",
                "notification",
                None,
                Some("notifications/tools/list_changed"),
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                    "params": {
                        "_meta": {"io.modelcontextprotocol/subscriptionId": "listen-1"}
                    }
                }),
            ),
        ];

        let expectation = subscription_expectation(&source, &source[0], &request).unwrap();
        assert_eq!(expectation.expected_frames, 2);
        assert_eq!(
            expectation.accepted_filter,
            SubscriptionFilter {
                tools_list_changed: true,
                prompts_list_changed: false,
                resources_list_changed: false,
                resource_subscriptions: BTreeSet::new(),
            }
        );
        assert!(validate_subscription_notification(
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/prompts/list_changed",
                "params": {
                    "_meta": {"io.modelcontextprotocol/subscriptionId": "listen-1"}
                }
            }),
            &expectation.subscription_id,
            &expectation.accepted_filter,
            1,
        )
        .is_err());
        assert!(validate_subscription_acknowledgment(
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/subscriptions/acknowledged",
                "params": {
                    "notifications": {"promptsListChanged": true},
                    "_meta": {"io.modelcontextprotocol/subscriptionId": "listen-1"}
                }
            }),
            &expectation.subscription_id,
            &expectation.filter,
            &expectation.accepted_filter,
        )
        .is_err());
        let resource_filter = SubscriptionFilter {
            tools_list_changed: false,
            prompts_list_changed: false,
            resources_list_changed: false,
            resource_subscriptions: BTreeSet::from(["file:///allowed".to_string()]),
        };
        assert!(validate_subscription_notification(
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/resources/updated",
                "params": {
                    "uri": "file:///outside",
                    "_meta": {"io.modelcontextprotocol/subscriptionId": "listen-1"}
                }
            }),
            &expectation.subscription_id,
            &resource_filter,
            1,
        )
        .is_err());

        let source_without_ack = vec![
            stored(
                0,
                "c2s",
                "request",
                Some("\"listen-1\""),
                Some("subscriptions/listen"),
                request.clone(),
            ),
            stored(
                2,
                "s2c",
                "notification",
                None,
                Some("notifications/tools/list_changed"),
                json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                    "params": {
                        "_meta": {"io.modelcontextprotocol/subscriptionId": "listen-1"}
                    }
                }),
            ),
        ];
        assert!(
            subscription_expectation(&source_without_ack, &source_without_ack[0], &request)
                .is_err()
        );
    }

    #[test]
    fn input_required_requires_valid_continuation_data() {
        let required = input_required_from_bytes(
            br#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"input_required","inputRequests":{"approval":{"method":"elicitation/create","params":{}}},"requestState":"opaque"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            required.input_response_keys,
            BTreeSet::from(["approval".to_string()])
        );
        assert_eq!(required.request_state.as_deref(), Some("opaque"));

        assert!(input_required_from_bytes(
            br#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"input_required"}}"#
        )
        .is_err());
        assert!(input_required_from_bytes(
            br#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"input_required","inputRequests":[]}}"#
        )
        .is_err());
    }

    #[test]
    fn mrtr_retry_reuses_captured_responses_but_live_request_state_and_fresh_id() {
        let original = json!({
            "jsonrpc": "2.0",
            "id": "first",
            "method": "tools/call",
            "params": {
                "name": "needs-input",
                "arguments": {"subject": "release"},
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
            }
        });
        let recorded_retry = json!({
            "jsonrpc": "2.0",
            "id": "second",
            "method": "tools/call",
            "params": {
                "name": "needs-input",
                "arguments": {"subject": "release"},
                "inputResponses": {"approval": {"action": "accept", "content": {"approved": true}}},
                "requestState": "captured-state",
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
            }
        });
        let input_required = InputRequired {
            input_response_keys: BTreeSet::from(["approval".to_string()]),
            request_state: Some("live-state".to_string()),
        };

        let retry = build_mrtr_retry_request(
            &original,
            &recorded_retry,
            &input_required,
            Value::String("fresh".to_string()),
        )
        .unwrap();
        assert_eq!(retry["id"], "fresh");
        assert_eq!(retry["params"]["requestState"], "live-state");
        assert_eq!(
            retry["params"]["inputResponses"],
            recorded_retry["params"]["inputResponses"]
        );
    }

    #[test]
    fn mrtr_retry_rejects_divergent_evidence() {
        let original = json!({
            "jsonrpc": "2.0",
            "id": "first",
            "method": "tools/call",
            "params": {
                "name": "needs-input",
                "arguments": {},
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
            }
        });
        let retry = json!({
            "jsonrpc": "2.0",
            "id": "second",
            "method": "tools/call",
            "params": {
                "name": "needs-input",
                "arguments": {},
                "inputResponses": {"wrong": {"action": "accept"}},
                "requestState": "captured",
                "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}
            }
        });
        let required = InputRequired {
            input_response_keys: BTreeSet::from(["approval".to_string()]),
            request_state: Some("live".to_string()),
        };
        assert!(build_mrtr_retry_request(&original, &retry, &required, json!("fresh")).is_err());
    }
    #[test]
    fn parse_header_splits_name_and_trims_value() {
        let (name, value) = parse_header("X-Api-Key:  secret-value ").unwrap();
        assert_eq!(name, "X-Api-Key");
        assert_eq!(value, "secret-value");
    }

    #[test]
    fn parse_header_rejects_missing_colon() {
        assert!(parse_header("X-Api-Key secret").is_err());
    }

    #[test]
    fn parse_header_rejects_reserved_headers_case_insensitively() {
        assert!(parse_header("Content-Type: text/plain").is_err());
        assert!(parse_header("mcp-session-id: abc").is_err());
        assert!(parse_header("MCP-Protocol-Version: 2026-07-28").is_err());
        assert!(parse_header("Mcp-Method: tools/list").is_err());
        assert!(parse_header("Mcp-Name: echo").is_err());
        assert!(parse_header("Mcp-Param-Region: us-west1").is_ok());
        assert!(parse_header("Host: example.test").is_err());
    }

    #[test]
    fn request_headers_carry_extra_headers_and_established_session_id() {
        let mut extra = HeaderMap::new();
        extra.insert("x-api-key", "secret".parse().unwrap());

        let headers = build_request_headers(
            &extra,
            Some("session-123"),
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
            &[],
        )
        .unwrap();

        assert_eq!(headers["x-api-key"], "secret");
        assert_eq!(headers["content-type"], "application/json");
        assert_eq!(headers["mcp-session-id"], "session-123");
    }

    #[test]
    fn request_headers_omit_session_id_before_it_is_established() {
        let headers = build_request_headers(
            &HeaderMap::new(),
            None,
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}),
            &[],
        )
        .unwrap();
        assert!(!headers.contains_key("mcp-session-id"));
    }

    #[test]
    fn modern_request_headers_are_derived_from_the_body_and_do_not_send_a_session_id() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "echo 世界",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });

        let headers =
            build_request_headers(&HeaderMap::new(), Some("legacy-id"), &request, &[]).unwrap();

        assert_eq!(headers["mcp-protocol-version"], "2026-07-28");
        assert_eq!(headers["mcp-method"], "tools/call");
        assert_eq!(headers["mcp-name"], "=?base64?ZWNobyDkuJbnlYw=?=");
        assert!(!headers.contains_key("mcp-session-id"));
    }

    #[test]
    fn modern_name_encoding_handles_plain_whitespace_and_sentinel_values() {
        assert_eq!(encode_mcp_header_value("echo"), "echo");
        assert_eq!(
            encode_mcp_header_value(" padded "),
            "=?base64?IHBhZGRlZCA=?="
        );
        assert_eq!(
            encode_mcp_header_value("=?base64?literal?="),
            "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="
        );
    }

    #[test]
    fn truncate_for_error_leaves_short_bodies_untouched() {
        assert_eq!(truncate_for_error("short body"), "short body");
    }

    #[test]
    fn truncate_for_error_truncates_long_bodies_on_char_boundaries() {
        let long_body = "x".repeat(600);
        let truncated = truncate_for_error(&long_body);
        assert!(truncated.ends_with("... (truncated)"));
        assert!(truncated.len() < long_body.len());
    }
}
