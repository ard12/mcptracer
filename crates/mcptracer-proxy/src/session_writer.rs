//! Shared machinery for driving an MCP stdio subprocess and recording its
//! traffic: subprocess spawn, the single storage-writer thread, and the
//! frame drain/forward loop. `record` and `replay` both build on this so the
//! writer-thread and redaction wiring stay identical between the two.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use mcptracer_protocol::{
    parse_stdio_frame, Direction, McpMessage, StdioFrame, StdioTransport, Transport,
};
use mcptracer_redact::{is_sensitive_key, Redactor, REDACTED_PLACEHOLDER};
use mcptracer_storage::Store;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tracing::warn;

/// Cap on bytes buffered while waiting for a frame's terminating newline. A
/// single MCP stdio frame larger than this is treated as a protocol error:
/// the affected direction stops pumping rather than growing memory without
/// bound. Frames already drained and forwarded are unaffected.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound on the source-payload bytes held by the asynchronous storage
/// queue. The existing channel capacity remains a second bound on event count.
pub const STORAGE_QUEUE_MAX_BYTES: usize = 16 * 1024 * 1024;
const STORAGE_EVENT_OVERHEAD_BYTES: usize = 1024;
const STORAGE_WRITE_BATCH_MESSAGES: usize = 128;
type StorageClose = (i64, u64, mpsc::Sender<std::result::Result<(), String>>);

pub enum StorageEvent {
    Message {
        message: McpMessage,
        reserved_bytes: usize,
    },
    Close {
        ended_at_ns: i64,
        dropped_messages: u64,
        done_tx: mpsc::Sender<std::result::Result<(), String>>,
    },
}

/// Lock-free byte accounting shared by both forwarding pumps and the single
/// storage writer. It bounds queued source payload bytes without turning the
/// forwarding path into a blocking operation.
pub struct StorageQueueBudget {
    max_bytes: usize,
    queued_bytes: AtomicUsize,
}

impl StorageQueueBudget {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            queued_bytes: AtomicUsize::new(0),
        }
    }

    fn try_reserve(&self, bytes: usize) -> bool {
        let Some(limit) = self.max_bytes.checked_sub(bytes) else {
            return false;
        };
        let mut current = self.queued_bytes.load(Ordering::Acquire);
        loop {
            if current > limit {
                return false;
            }
            match self.queued_bytes.compare_exchange_weak(
                current,
                current + bytes,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn release(&self, bytes: usize) {
        self.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
    }

    #[cfg(test)]
    fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CaptureStamp {
    seq: u64,
    timestamp_ns: i64,
}

/// Spawn the MCP server subprocess with stdin/stdout piped and stderr
/// inherited, so its diagnostics reach the terminal without polluting the
/// stdio transport.
pub fn spawn_server_process(server_args: &[String]) -> Result<Child> {
    let server_program = server_args
        .first()
        .ok_or_else(|| anyhow!("missing MCP server command after --"))?;

    Command::new(server_program)
        .args(&server_args[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to spawn MCP server program: {server_program}"))
}

/// Render a server command for session metadata without retaining common
/// secret-bearing argument values. The subprocess still receives `server_args`
/// unchanged; this only protects the stored/displayed command string.
pub fn redact_server_command(server_args: &[String]) -> String {
    let mut redact_next = false;
    let mut rendered = Vec::with_capacity(server_args.len());

    for arg in server_args {
        if redact_next {
            rendered.push(REDACTED_PLACEHOLDER.to_string());
            redact_next = false;
            continue;
        }

        if let Some((key, _)) = arg.split_once('=') {
            if is_sensitive_key(key) {
                rendered.push(format!("{key}={REDACTED_PLACEHOLDER}"));
                continue;
            }
        }

        if arg.starts_with('-') && is_sensitive_key(arg) {
            redact_next = true;
        }
        rendered.push(arg.clone());
    }

    rendered.join(" ")
}

fn collect_storage_event(
    event: StorageEvent,
    messages: &mut Vec<(McpMessage, usize)>,
    close: &mut Option<StorageClose>,
) {
    match event {
        StorageEvent::Message {
            message,
            reserved_bytes,
        } => messages.push((message, reserved_bytes)),
        StorageEvent::Close {
            ended_at_ns,
            dropped_messages,
            done_tx,
        } => *close = Some((ended_at_ns, dropped_messages, done_tx)),
    }
}

/// The single thread that owns the `Store` handle for a session. All writes
/// (messages, final dropped-message accounting, session close) funnel through
/// this one thread so SQLite access is never contended across threads.
pub fn spawn_storage_writer(
    mut store: Store,
    session_id: String,
    redactor: Redactor,
    rx: Receiver<StorageEvent>,
    queue_budget: Arc<StorageQueueBudget>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut failed_message_records = 0_u64;
        let mut first_failure: Option<String> = None;

        while let Ok(event) = rx.recv() {
            let mut messages = Vec::with_capacity(STORAGE_WRITE_BATCH_MESSAGES);
            let mut close = None;

            collect_storage_event(event, &mut messages, &mut close);
            while close.is_none() && messages.len() < STORAGE_WRITE_BATCH_MESSAGES {
                match rx.try_recv() {
                    Ok(event) => collect_storage_event(event, &mut messages, &mut close),
                    Err(_) => break,
                }
            }

            if !messages.is_empty() {
                for (message, _) in &mut messages {
                    redactor.redact(&mut message.payload);
                }
                if let Err(err) =
                    store.write_messages(&session_id, messages.iter().map(|(message, _)| message))
                {
                    warn!("failed to write MCP message batch: {err}");
                    failed_message_records = failed_message_records
                        .saturating_add(u64::try_from(messages.len()).unwrap_or(u64::MAX));
                    first_failure.get_or_insert_with(|| {
                        "failed to persist one or more MCP message records".to_string()
                    });
                }
                for (_, reserved_bytes) in messages {
                    queue_budget.release(reserved_bytes);
                }
            }

            let Some((ended_at_ns, dropped_messages, done_tx)) = close else {
                continue;
            };

            let total_dropped = dropped_messages.saturating_add(failed_message_records);
            if total_dropped > 0 {
                if let Err(err) = store.increment_dropped_messages(&session_id, total_dropped) {
                    warn!("failed to update dropped message count: {err}");
                    first_failure.get_or_insert_with(|| {
                        "failed to persist dropped-message accounting".to_string()
                    });
                } else {
                    first_failure.get_or_insert_with(|| {
                        format!("capture lost {total_dropped} message record(s)")
                    });
                }
            }
            if let Err(err) = store.close_session(&session_id, ended_at_ns) {
                warn!("failed to close session: {err}");
                first_failure
                    .get_or_insert_with(|| "failed to finalize the recorded session".to_string());
            }
            let _ = done_tx.send(match first_failure {
                Some(message) => Err(message),
                None => Ok(()),
            });
            break;
        }
    })
}

/// Close the storage writer, wait for its final session accounting, and turn
/// any recording loss or SQLite failure into a command failure. The caller
/// supplies the only remaining sender after its pump tasks have exited.
pub fn finish_storage_writer(
    storage_tx: SyncSender<StorageEvent>,
    storage_handle: thread::JoinHandle<()>,
    ended_at_ns: i64,
    dropped_messages: u64,
) -> Result<()> {
    let (done_tx, done_rx) = mpsc::channel();
    storage_tx
        .send(StorageEvent::Close {
            ended_at_ns,
            dropped_messages,
            done_tx,
        })
        .context("storage writer stopped before the session could be finalized")?;
    drop(storage_tx);

    let result = done_rx
        .recv()
        .context("storage writer stopped without finalizing the session")?;
    storage_handle
        .join()
        .map_err(|_| anyhow!("storage writer thread panicked"))?;
    result.map_err(|message| anyhow!(message))
}

/// Drain complete stdio frames from `buf`, forwarding each to `writer` and
/// recording it, until `buf` holds no more complete frames. The sequence and
/// timestamp are reserved before the asynchronous write, so the two direction
/// pumps have one stable ordering source while still forwarding before parsing
/// or recording. Returns `false` if forwarding should stop (write/flush error
/// or unparseable frame).
pub async fn drain_frames<W>(
    buf: &mut Vec<u8>,
    direction: Direction,
    writer: &mut W,
    storage_tx: &SyncSender<StorageEvent>,
    seq: &AtomicU64,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
) -> bool
where
    W: AsyncWrite + Unpin,
{
    loop {
        let frame = match parse_stdio_frame(buf) {
            Ok(Some(frame)) => frame,
            Ok(None) => return true,
            Err(err) => {
                warn!("failed to parse stdio frame: {err}");
                return false;
            }
        };

        let stamp = reserve_capture_stamp(seq);

        if let Err(err) = writer.write_all(&frame.wire_bytes).await {
            warn!("failed to forward MCP frame: {err}");
            return false;
        }
        if let Err(err) = writer.flush().await {
            warn!("failed to flush MCP frame: {err}");
            return false;
        }

        let consumed = frame.consumed;
        record_frame(
            frame,
            direction,
            storage_tx,
            stamp,
            dropped_messages,
            queue_budget,
        );
        buf.drain(..consumed);
    }
}

fn reserve_capture_stamp(seq: &AtomicU64) -> CaptureStamp {
    CaptureStamp {
        seq: seq.fetch_add(1, Ordering::Relaxed),
        timestamp_ns: now_ns(),
    }
}

fn record_frame(
    frame: StdioFrame,
    direction: Direction,
    storage_tx: &SyncSender<StorageEvent>,
    stamp: CaptureStamp,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
) {
    match StdioTransport.decode_message(&frame.json_bytes, direction, stamp.seq, stamp.timestamp_ns)
    {
        Ok(msg) => {
            try_record(storage_tx, msg, dropped_messages, queue_budget);
        }
        Err(err) => {
            // Forwarding already happened (forward-before-record): the client
            // and server are unaffected. But this frame is now a gap in the
            // stored evidence, so it must count as dropped like any other
            // recording loss — otherwise a session missing this exact frame
            // would still report zero drops and pass validate/diff/assert as
            // healthy.
            dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("forwarded non-JSON MCP frame but did not record it: {err}");
        }
    }
}

pub fn try_record(
    storage_tx: &SyncSender<StorageEvent>,
    message: McpMessage,
    dropped_messages: &AtomicU64,
    queue_budget: &StorageQueueBudget,
) {
    let reserved_bytes = message
        .payload_bytes
        .saturating_add(STORAGE_EVENT_OVERHEAD_BYTES);
    if !queue_budget.try_reserve(reserved_bytes) {
        dropped_messages.fetch_add(1, Ordering::Relaxed);
        warn!("storage queue byte budget exhausted; dropped MCP message record");
        return;
    }

    match storage_tx.try_send(StorageEvent::Message {
        message,
        reserved_bytes,
    }) {
        Ok(()) => {}
        Err(TrySendError::Full(StorageEvent::Message { reserved_bytes, .. })) => {
            queue_budget.release(reserved_bytes);
            dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("storage channel full; dropped MCP message record");
        }
        Err(TrySendError::Disconnected(StorageEvent::Message { reserved_bytes, .. })) => {
            queue_budget.release(reserved_bytes);
            dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("storage writer disconnected; dropped MCP message record");
        }
        Err(_) => unreachable!("try_record only sends message events"),
    }
}

pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos() as i64
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc};

    use mcptracer_protocol::Direction;
    use mcptracer_redact::{RedactionPolicy, Redactor};
    use mcptracer_storage::Store;
    use serde_json::json;

    use mcptracer_protocol::StdioFrame;

    use super::{
        finish_storage_writer, record_frame, redact_server_command, reserve_capture_stamp,
        spawn_storage_writer, try_record, CaptureStamp, McpMessage, StorageQueueBudget,
        STORAGE_QUEUE_MAX_BYTES,
    };

    fn message(seq: u64) -> McpMessage {
        McpMessage {
            seq,
            timestamp_ns: 0,
            direction: Direction::ClientToServer,
            payload: json!({"jsonrpc":"2.0","id":seq,"method":"initialize"}),
            payload_bytes: 48,
        }
    }

    #[test]
    fn byte_budget_admits_exactly_the_real_cap_and_refuses_one_byte_past_it() {
        // The other budget tests substitute small synthetic ceilings (1_500 and
        // 4_096 bytes). That proves the mechanism but never the constant the
        // proxy actually runs with, so an off-by-one introduced at the real
        // limit would not be caught. Exercise STORAGE_QUEUE_MAX_BYTES itself.
        let budget = StorageQueueBudget::new(STORAGE_QUEUE_MAX_BYTES);

        // Exactly at the cap must be admitted - the cap is inclusive, so a
        // payload sized precisely to it is valid traffic, not an overflow.
        assert!(budget.try_reserve(STORAGE_QUEUE_MAX_BYTES));
        assert_eq!(budget.queued_bytes(), STORAGE_QUEUE_MAX_BYTES);
        assert!(
            !budget.try_reserve(1),
            "a budget reserved to its cap must refuse even a single further byte"
        );

        budget.release(STORAGE_QUEUE_MAX_BYTES);
        assert_eq!(budget.queued_bytes(), 0);

        // Just under, then the byte that lands exactly on the cap.
        assert!(budget.try_reserve(STORAGE_QUEUE_MAX_BYTES - 1));
        assert!(
            budget.try_reserve(1),
            "the final byte up to the cap must still fit"
        );
        assert!(!budget.try_reserve(1));

        // A single reservation larger than the whole budget can never fit, and
        // must fail rather than overflow the subtraction that computes room.
        let fresh = StorageQueueBudget::new(STORAGE_QUEUE_MAX_BYTES);
        assert!(!fresh.try_reserve(STORAGE_QUEUE_MAX_BYTES + 1));
        assert!(!fresh.try_reserve(usize::MAX));
        assert_eq!(fresh.queued_bytes(), 0);
    }

    #[test]
    fn full_storage_queue_counts_dropped_messages_out_of_band() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let dropped = AtomicU64::new(0);
        let budget = StorageQueueBudget::new(4 * 1024);

        try_record(&tx, message(0), &dropped, &budget);
        try_record(&tx, message(1), &dropped, &budget);

        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(
            budget.queued_bytes(),
            48 + super::STORAGE_EVENT_OVERHEAD_BYTES
        );
    }

    #[test]
    fn byte_budget_drops_records_before_message_count_capacity() {
        let (tx, rx) = mpsc::sync_channel(2);
        let dropped = AtomicU64::new(0);
        let budget = StorageQueueBudget::new(1_500);

        try_record(&tx, message(0), &dropped, &budget);
        try_record(&tx, message(1), &dropped, &budget);

        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(rx.try_iter().count(), 1);
    }

    #[test]
    fn non_json_frame_is_forwarded_but_counted_as_dropped() {
        // Forward-before-record means a non-JSON frame still reaches the
        // wire; but it must count as a recording loss (see the comment on
        // record_frame) so validate/diff/assert cannot see a session
        // missing this frame as healthy.
        let (tx, _rx) = mpsc::sync_channel(4);
        let dropped = AtomicU64::new(0);
        let budget = StorageQueueBudget::new(4 * 1024);
        let frame = StdioFrame {
            json_bytes: b"not json".to_vec(),
            wire_bytes: b"not json\n".to_vec(),
            consumed: 9,
        };

        record_frame(
            frame,
            Direction::ClientToServer,
            &tx,
            CaptureStamp {
                seq: 0,
                timestamp_ns: 0,
            },
            &dropped,
            &budget,
        );

        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn reserves_monotonic_capture_stamps_before_forwarding() {
        let seq = AtomicU64::new(41);

        let first = reserve_capture_stamp(&seq);
        let second = reserve_capture_stamp(&seq);

        assert_eq!(first.seq, 41);
        assert_eq!(second.seq, 42);
        assert!(first.timestamp_ns > 0);
        assert!(second.timestamp_ns >= first.timestamp_ns);
    }

    #[test]
    fn redacts_sensitive_server_command_arguments_in_session_metadata() {
        let args = vec![
            "server".to_string(),
            "--api-key".to_string(),
            "sk-live".to_string(),
            "ACCESS_TOKEN=top-secret".to_string(),
            "--port=3000".to_string(),
        ];

        let command = redact_server_command(&args);

        assert_eq!(
            command,
            "server --api-key ***REDACTED*** ACCESS_TOKEN=***REDACTED*** --port=3000"
        );
        assert!(!command.contains("sk-live"));
        assert!(!command.contains("top-secret"));
    }

    #[test]
    fn writer_reports_sqlite_message_failures_at_session_close() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store.create_session("test", "server", "stdio", 0).unwrap();
        let (tx, rx) = mpsc::sync_channel(4);
        let budget = Arc::new(StorageQueueBudget::new(4 * 1024));
        let writer = spawn_storage_writer(
            store,
            session_id,
            Redactor::new(RedactionPolicy::None),
            rx,
            Arc::clone(&budget),
        );

        let dropped = AtomicU64::new(0);
        try_record(&tx, message(0), &dropped, &budget);
        try_record(&tx, message(0), &dropped, &budget);

        let err = finish_storage_writer(tx, writer, 1, 0).unwrap_err();
        assert!(err
            .to_string()
            .contains("failed to persist one or more MCP message records"));
        assert_eq!(budget.queued_bytes(), 0);
    }
}
