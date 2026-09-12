use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use mcptracer_model::correlate;
use mcptracer_model::plan::{build_plan, driven_messages, tool_is_allowed};
use mcptracer_protocol::{
    encode_stdio_frame, parse_json_payload, parse_stdio_frame, Direction, McpMessage, MessageKind,
};
use mcptracer_redact::{RedactionPolicy, Redactor, REDACTED_PLACEHOLDER};
use mcptracer_storage::{Store, StoredMessage};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, ChildStdout};

use crate::session_writer::{
    finish_storage_writer, now_ns, redact_server_command, spawn_server_process,
    spawn_storage_writer, try_record, StorageEvent, StorageQueueBudget, STORAGE_QUEUE_MAX_BYTES,
};

#[derive(Args)]
pub struct ReplayArgs {
    /// Id (or unique prefix) of the recorded session to replay.
    pub session_id: String,

    #[arg(long, default_value = "unknown")]
    pub client: String,

    /// Redaction policy applied to the NEW session's stored payloads.
    /// Forwarded bytes are never modified.
    #[arg(long, default_value = "none")]
    pub redact: String,

    /// Extra JSON field names to mask, comma-separated. Requires
    /// `--redact default` and is stored as the `custom` policy.
    #[arg(long, value_delimiter = ',')]
    pub redact_keys: Vec<String>,

    /// `fast`: send the next client message as soon as the previous
    /// correlated response arrives. `realtime`: reproduce the source
    /// session's inter-message gaps.
    #[arg(long, value_enum, default_value_t = TimingMode::Fast)]
    pub timing: TimingMode,

    /// Milliseconds to wait for a response before recording a replayed
    /// request as unanswered and moving on.
    #[arg(long, default_value_t = 30_000)]
    pub request_timeout: u64,

    /// Abort the replay instead of auto-responding when the server sends a
    /// request the source session cannot answer.
    #[arg(long)]
    pub strict_server_requests: bool,

    /// Acknowledge that replay re-executes every recorded request for real
    /// against the target server (deletes, sends, payments, and any other
    /// side effect included — this is not a preview), and that a source
    /// session recorded with redaction sends the literal `***REDACTED***`
    /// placeholder as the argument value rather than the real secret.
    /// Suppresses both warnings printed before every run.
    #[arg(long)]
    pub i_understand_side_effects: bool,

    /// Print a JSON report of the calls this replay would make (methods,
    /// tools, redacted placeholders, known risk annotations) and exit
    /// without spawning a server process or sending anything. The trailing
    /// server command is not required with this flag.
    #[arg(long)]
    pub plan: bool,

    /// Only send `tools/call` requests for these tool names (repeatable or
    /// comma-separated). Non-tool protocol messages (`initialize`,
    /// notifications) are never filtered.
    #[arg(long, value_delimiter = ',')]
    pub allow_tool: Vec<String>,

    /// Never send `tools/call` requests for these tool names (repeatable or
    /// comma-separated). Takes precedence over `--allow-tool`.
    #[arg(long, value_delimiter = ',')]
    pub deny_tool: Vec<String>,

    #[arg(trailing_var_arg = true)]
    pub server_args: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum TimingMode {
    Fast,
    Realtime,
}

pub async fn run(args: ReplayArgs, db_path: PathBuf) -> Result<()> {
    if !args.plan && args.server_args.is_empty() {
        return Err(anyhow!(
            "missing MCP server command after -- (required unless --plan is set)"
        ));
    }

    let store = Store::open(&db_path)?;
    let source_messages = store.get_messages(&args.session_id)?;
    if source_messages.is_empty() {
        return Err(anyhow!(
            "source session has no recorded messages: {}",
            args.session_id
        ));
    }

    let plan = build_plan(&source_messages, &args.allow_tool, &args.deny_tool);
    if args.plan {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
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
    let response_lookaside = server_initiated_responses(&source_messages);
    warn_side_effects(&driven, args.i_understand_side_effects);
    warn_redacted_replay(&driven, args.i_understand_side_effects);

    let policy = RedactionPolicy::from_cli(&args.redact, &args.redact_keys)
        .map_err(|error| anyhow!(error))?;
    let redactor = Redactor::new(policy.clone());

    let server_command = redact_server_command(&args.server_args);
    let mut server_process = spawn_server_process(&args.server_args)?;
    let server_stdin = server_process
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open server stdin"))?;
    let server_stdout = server_process
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to open server stdout"))?;

    let target_session_id =
        store.create_session(&args.client, &server_command, "stdio", now_ns())?;
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
        "[mcptracer] replaying session {} as {} -> {}",
        args.session_id,
        target_session_id,
        db_path.display()
    );

    let seq = Arc::new(AtomicU64::new(0));
    let dropped_messages = AtomicU64::new(0);
    let outcome = drive(
        &args,
        server_stdin,
        server_stdout,
        &driven,
        &response_lookaside,
        &seq,
        &storage_tx,
        &dropped_messages,
        &queue_budget,
    )
    .await;

    let _ = server_process.kill().await;
    let _ = server_process.wait().await;
    let storage_result = finish_storage_writer(
        storage_tx,
        storage_handle,
        now_ns(),
        dropped_messages.load(Ordering::Relaxed),
    );

    match (outcome, storage_result) {
        (Ok(()), Ok(())) => {
            eprintln!("[mcptracer] replay session {} ended", target_session_id);
            Ok(())
        }
        (Err(err), _) | (_, Err(err)) => {
            eprintln!(
                "[mcptracer] replay session {} aborted: {}",
                target_session_id, err
            );
            Err(err)
        }
    }
}

/// Print an unmissable, non-blocking warning naming every distinct call
/// replay is about to re-execute for real against the live target — replay
/// is not a preview, so a recorded destructive call (delete, send, payment,
/// ...) fires again exactly as recorded. Silences with
/// `--i-understand-side-effects`.
pub(crate) fn warn_side_effects(driven: &[&StoredMessage], suppress: bool) {
    if suppress {
        return;
    }
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for message in driven {
        let label = match (&message.method, &message.tool_name) {
            (Some(method), Some(tool)) => format!("{method} {tool}"),
            (Some(method), None) => method.clone(),
            (None, _) => continue,
        };
        *counts.entry(label).or_insert(0) += 1;
    }
    if counts.is_empty() {
        return;
    }
    eprintln!(
        "[mcptracer] WARNING: replay re-executes {} recorded call(s) against the live target:",
        driven.len()
    );
    for (label, count) in &counts {
        eprintln!("  {label} (x{count})");
    }
    eprintln!(
        "[mcptracer] This has real side effects — not a preview. Pass --i-understand-side-effects to suppress this warning."
    );
}

/// Print an unmissable, non-blocking warning naming every distinct call whose
/// recorded payload still contains the `***REDACTED***` placeholder — replay
/// sends the stored payload as-is, so a redacted argument is sent to the live
/// target verbatim as the literal placeholder string, not the original
/// secret. This is a faithfulness problem (the live server sees garbage
/// where a real value belongs), not just a confidentiality one. Detects by
/// scanning the actual payload rather than trusting the session's recorded
/// redaction policy, since a `custom` policy may only mask specific keys and
/// leave most driven messages untouched. Silences with
/// `--i-understand-side-effects`, same flag as `warn_side_effects`.
pub(crate) fn warn_redacted_replay(driven: &[&StoredMessage], suppress: bool) {
    if suppress {
        return;
    }
    let counts = redacted_replay_counts(driven);
    if counts.is_empty() {
        return;
    }
    eprintln!(
        "[mcptracer] WARNING: {} recorded call(s) still contain the \"{REDACTED_PLACEHOLDER}\" placeholder and will send it verbatim, not the original value:",
        counts.values().sum::<usize>()
    );
    for (label, count) in &counts {
        eprintln!("  {label} (x{count})");
    }
    eprintln!(
        "[mcptracer] The live target will see the literal placeholder string, not the redacted secret — this is very unlikely to work as intended. Pass --i-understand-side-effects to suppress this warning."
    );
}

/// Distinct method/tool labels among `driven` whose stored payload contains
/// the redaction placeholder, with per-label occurrence counts.
fn redacted_replay_counts(driven: &[&StoredMessage]) -> std::collections::BTreeMap<String, usize> {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for message in driven {
        if !message.payload.contains(REDACTED_PLACEHOLDER) {
            continue;
        }
        let label = match (&message.method, &message.tool_name) {
            (Some(method), Some(tool)) => format!("{method} {tool}"),
            (Some(method), None) => method.clone(),
            (None, _) => continue,
        };
        *counts.entry(label).or_insert(0) += 1;
    }
    counts
}

/// Maps a server-initiated request's `rpc_id` to the raw payload of the
/// source session's `c2s` response, so replay can answer the same way the
/// original client did. Built via `correlate` so id matching stays scoped to
/// the responder direction, exactly like the live model.
fn server_initiated_responses(source: &[StoredMessage]) -> HashMap<String, String> {
    let model = correlate(source);
    let mut map = HashMap::new();
    for exchange in &model.exchanges {
        if exchange.origin != Direction::ServerToClient {
            continue;
        }
        let (Some(rpc_id), Some(response_seq)) = (&exchange.rpc_id, exchange.response_seq) else {
            continue;
        };
        if let Some(msg) = source.iter().find(|m| m.seq == response_seq) {
            map.insert(rpc_id.clone(), msg.payload.clone());
        }
    }
    map
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    args: &ReplayArgs,
    mut server_stdin: ChildStdin,
    mut server_stdout: ChildStdout,
    driven: &[&StoredMessage],
    response_lookaside: &HashMap<String, String>,
    seq: &AtomicU64,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut prev_ts_ns: Option<i64> = None;
    let request_timeout = Duration::from_millis(args.request_timeout);

    for msg in driven {
        if args.timing == TimingMode::Realtime {
            if let Some(prev) = prev_ts_ns {
                let delta_ns = msg.ts_ns - prev;
                if delta_ns > 0 {
                    tokio::time::sleep(Duration::from_nanos(delta_ns as u64)).await;
                }
            }
        }
        prev_ts_ns = Some(msg.ts_ns);

        let value: Value = serde_json::from_str(&msg.payload)
            .with_context(|| format!("source message seq {} is not valid JSON", msg.seq))?;
        send_frame(&mut server_stdin, &value).await?;

        let sent = McpMessage {
            seq: seq.fetch_add(1, Ordering::Relaxed),
            timestamp_ns: now_ns(),
            direction: Direction::ClientToServer,
            payload_bytes: value.to_string().len(),
            payload: value,
        };
        let is_request = sent.kind() == MessageKind::Request;
        let rpc_id = sent.id_json();
        try_record(storage_tx, sent, dropped_messages, queue_budget);

        if !is_request {
            continue;
        }
        let Some(rpc_id) = rpc_id else { continue };

        match wait_for_response(
            &mut server_stdout,
            &mut server_stdin,
            &mut buf,
            seq,
            storage_tx,
            dropped_messages,
            queue_budget,
            &rpc_id,
            response_lookaside,
            args.strict_server_requests,
            request_timeout,
        )
        .await?
        {
            true => {}
            false => {
                tracing::warn!(
                    "replayed request (id={}) went unanswered within {}ms",
                    rpc_id,
                    args.request_timeout
                );
            }
        }
    }

    Ok(())
}

async fn send_frame(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let bytes = encode_stdio_frame(value);
    stdin
        .write_all(&bytes)
        .await
        .context("failed to write to server stdin (server may have exited)")?;
    stdin
        .flush()
        .await
        .context("failed to flush server stdin")?;
    Ok(())
}

/// Read server stdout until a response matching `expected_rpc_id` arrives (in
/// the `s2c` direction), the timeout elapses, or the server closes stdout.
/// Every frame observed along the way is recorded; server-initiated requests
/// are answered from `response_lookaside` as they're seen.
#[allow(clippy::too_many_arguments)]
async fn wait_for_response(
    stdout: &mut ChildStdout,
    stdin: &mut ChildStdin,
    buf: &mut Vec<u8>,
    seq: &AtomicU64,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
    expected_rpc_id: &str,
    response_lookaside: &HashMap<String, String>,
    strict_server_requests: bool,
    timeout: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        while let Some(frame) = parse_stdio_frame(buf)? {
            let consumed = frame.consumed;
            let value = parse_json_payload(&frame.json_bytes)?;
            let msg = McpMessage {
                seq: seq.fetch_add(1, Ordering::Relaxed),
                timestamp_ns: now_ns(),
                direction: Direction::ServerToClient,
                payload_bytes: frame.json_bytes.len(),
                payload: value.clone(),
            };
            let kind = msg.kind();
            let matched =
                kind == MessageKind::Response && msg.id_json().as_deref() == Some(expected_rpc_id);
            try_record(storage_tx, msg, dropped_messages, queue_budget);
            buf.drain(..consumed);

            if kind == MessageKind::Request {
                handle_server_request(
                    stdin,
                    seq,
                    storage_tx,
                    dropped_messages,
                    queue_budget,
                    &value,
                    response_lookaside,
                    strict_server_requests,
                )
                .await?;
            }
            if matched {
                return Ok(true);
            }
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }

        let mut tmp = [0_u8; 4096];
        match tokio::time::timeout(remaining, stdout.read(&mut tmp)).await {
            Ok(Ok(0)) => return Ok(false),
            Ok(Ok(n)) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > crate::session_writer::MAX_FRAME_BYTES {
                    return Err(anyhow!(
                        "server frame exceeded {} bytes without a newline",
                        crate::session_writer::MAX_FRAME_BYTES
                    ));
                }
            }
            Ok(Err(err)) => return Err(anyhow!("failed to read server stdout: {err}")),
            Err(_) => return Ok(false),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_server_request(
    stdin: &mut ChildStdin,
    seq: &AtomicU64,
    storage_tx: &SyncSender<StorageEvent>,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
    request_value: &Value,
    response_lookaside: &HashMap<String, String>,
    strict: bool,
) -> Result<()> {
    let Some(id_json) = request_value.get("id").map(Value::to_string) else {
        return Ok(());
    };

    let response_value: Value = if let Some(payload) = response_lookaside.get(&id_json) {
        serde_json::from_str(payload).context("stored source response is not valid JSON")?
    } else if strict {
        return Err(anyhow!(
            "server sent a request the source session never answered (method={:?}, id={}) and --strict-server-requests is set",
            request_value.get("method").and_then(Value::as_str),
            id_json
        ));
    } else {
        tracing::warn!(
            "server sent a request mcptracer cannot answer from the source session (id={}); replying with a not-supported error",
            id_json
        );
        json!({
            "jsonrpc": "2.0",
            "id": request_value.get("id").cloned().unwrap_or(Value::Null),
            "error": {
                "code": -32601,
                "message": "method not supported by mcptracer replay",
            }
        })
    };

    send_frame(stdin, &response_value).await?;
    let msg = McpMessage {
        seq: seq.fetch_add(1, Ordering::Relaxed),
        timestamp_ns: now_ns(),
        direction: Direction::ClientToServer,
        payload_bytes: response_value.to_string().len(),
        payload: response_value,
    };
    try_record(storage_tx, msg, dropped_messages, queue_budget);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(
        seq: u64,
        ts_ns: i64,
        direction: &str,
        kind: &str,
        rpc_id: Option<&str>,
        method: Option<&str>,
    ) -> StoredMessage {
        StoredMessage {
            seq,
            ts_ns,
            direction: direction.to_string(),
            message_kind: kind.to_string(),
            rpc_id: rpc_id.map(String::from),
            method: method.map(String::from),
            tool_name: None,
            payload: "{}".to_string(),
            payload_bytes: 2,
            is_error: false,
            error_code: None,
        }
    }

    #[test]
    fn driven_messages_excludes_server_traffic_and_client_responses() {
        let msgs = [
            stored(0, 0, "c2s", "request", Some("1"), Some("initialize")),
            stored(1, 10, "s2c", "response", Some("1"), None),
            stored(
                2,
                20,
                "c2s",
                "notification",
                None,
                Some("notifications/initialized"),
            ),
            stored(
                3,
                30,
                "s2c",
                "request",
                Some("9"),
                Some("sampling/createMessage"),
            ),
            stored(4, 40, "c2s", "response", Some("9"), None),
            stored(5, 50, "c2s", "request", Some("2"), Some("tools/list")),
        ];

        let driven = driven_messages(&msgs);
        let seqs: Vec<u64> = driven.iter().map(|m| m.seq).collect();
        assert_eq!(seqs, vec![0, 2, 5]);
    }

    #[test]
    fn driven_messages_forces_initialize_first() {
        let msgs = [
            stored(
                0,
                0,
                "c2s",
                "notification",
                None,
                Some("notifications/cancelled"),
            ),
            stored(1, 10, "c2s", "request", Some("1"), Some("initialize")),
        ];

        let driven = driven_messages(&msgs);
        assert_eq!(driven[0].method.as_deref(), Some("initialize"));
        assert_eq!(driven[1].seq, 0);
    }

    #[test]
    fn server_initiated_responses_maps_id_to_source_c2s_response() {
        let mut createmsg_response = stored(2, 20, "c2s", "response", Some("9"), None);
        createmsg_response.payload = r#"{"jsonrpc":"2.0","id":9,"result":{"ok":true}}"#.to_string();

        let msgs = [
            stored(
                0,
                0,
                "s2c",
                "request",
                Some("9"),
                Some("sampling/createMessage"),
            ),
            stored(1, 10, "c2s", "request", Some("1"), Some("initialize")),
            createmsg_response,
        ];

        let map = server_initiated_responses(&msgs);
        assert_eq!(
            map.get("9").map(String::as_str),
            Some(r#"{"jsonrpc":"2.0","id":9,"result":{"ok":true}}"#)
        );
        // The client-initiated exchange must not leak into the lookaside.
        assert!(!map.contains_key("1"));
    }

    #[test]
    fn redacted_replay_counts_flags_only_calls_containing_the_placeholder() {
        let mut clean_call = stored(0, 0, "c2s", "request", Some("1"), Some("tools/call"));
        clean_call.tool_name = Some("echo".to_string());
        clean_call.payload =
            r#"{"jsonrpc":"2.0","id":1,"params":{"name":"echo","arguments":{"text":"hi"}}}"#
                .to_string();

        let mut redacted_call = stored(1, 10, "c2s", "request", Some("2"), Some("tools/call"));
        redacted_call.tool_name = Some("send_email".to_string());
        redacted_call.payload = format!(
            r#"{{"jsonrpc":"2.0","id":2,"params":{{"name":"send_email","arguments":{{"api_key":"{REDACTED_PLACEHOLDER}"}}}}}}"#
        );

        let mut redacted_call_again =
            stored(2, 20, "c2s", "request", Some("3"), Some("tools/call"));
        redacted_call_again.tool_name = Some("send_email".to_string());
        redacted_call_again.payload = redacted_call.payload.clone();

        let driven = vec![&clean_call, &redacted_call, &redacted_call_again];
        let counts = redacted_replay_counts(&driven);

        assert_eq!(counts.len(), 1);
        assert_eq!(counts.get("tools/call send_email"), Some(&2));
        assert!(!counts.contains_key("tools/call echo"));
    }

    #[test]
    fn redacted_replay_counts_is_empty_when_nothing_is_redacted() {
        let call = stored(0, 0, "c2s", "request", Some("1"), Some("tools/list"));
        let driven = vec![&call];
        assert!(redacted_replay_counts(&driven).is_empty());
    }
}
