//! Shared machinery for driving an MCP stdio subprocess and recording its
//! traffic: subprocess spawn, the single storage-writer thread, and the
//! frame drain/forward loop. `record` and `replay` both build on this so the
//! writer-thread and redaction wiring stay identical between the two.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use mcptracer_protocol::{
    parse_stdio_frame, Direction, McpMessage, StdioFrame, StdioTransport, Transport,
};
use mcptracer_redact::{is_sensitive_key, RedactionContextTracker, Redactor, REDACTED_PLACEHOLDER};
use mcptracer_storage::Store;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tracing::warn;

/// Cap on bytes buffered while waiting for a frame's terminating newline. A
/// single MCP stdio frame larger than this is treated as a protocol error:
/// the affected direction stops pumping rather than growing memory without
/// bound. Frames already drained and forwarded are unaffected.
pub use mcptracer_protocol::MAX_FRAME_BYTES;

/// Upper bound on the source-payload bytes held by the asynchronous storage
/// queue. The existing channel capacity remains a second bound on event count.
pub const STORAGE_QUEUE_MAX_BYTES: usize = 16 * 1024 * 1024;
const STORAGE_EVENT_OVERHEAD_BYTES: usize = 1024;
const STORAGE_WRITE_BATCH_MESSAGES: usize = 128;
const STORAGE_REORDER_MAX_MESSAGES: usize = 4096;
type StorageClose = (i64, u64, mpsc::Sender<std::result::Result<(), String>>);

pub enum StorageEvent {
    Message {
        message: McpMessage,
        reservation: QueueReservation,
    },
    Close {
        ended_at_ns: i64,
        dropped_messages: u64,
        done_tx: mpsc::Sender<std::result::Result<(), String>>,
    },
    #[cfg(test)]
    TestBarrier {
        entered_tx: mpsc::Sender<()>,
        release_rx: mpsc::Receiver<()>,
    },
}

/// Lock-free byte accounting shared by both forwarding pumps and the single
/// storage writer. It bounds queued source payload bytes without turning the
/// forwarding path into a blocking operation.
pub struct StorageQueueBudget {
    max_bytes: usize,
    queued_bytes: Arc<AtomicUsize>,
}

/// Owns one byte reservation for exactly as long as its queued event lives.
/// Dropping an event on channel teardown, writer failure, or normal batch
/// completion returns the reservation once through Rust's ownership rules.
pub(crate) struct QueueReservation {
    queued_bytes: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for QueueReservation {
    fn drop(&mut self) {
        self.queued_bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl StorageQueueBudget {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            queued_bytes: Arc::new(AtomicUsize::new(0)),
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

    fn reserve_event(&self, bytes: usize) -> Option<QueueReservation> {
        self.try_reserve(bytes).then(|| QueueReservation {
            queued_bytes: Arc::clone(&self.queued_bytes),
            bytes,
        })
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
    messages: &mut Vec<(McpMessage, QueueReservation)>,
    close: &mut Option<StorageClose>,
) {
    match event {
        StorageEvent::Message {
            message,
            reservation,
        } => messages.push((message, reservation)),
        StorageEvent::Close {
            ended_at_ns,
            dropped_messages,
            done_tx,
        } => *close = Some((ended_at_ns, dropped_messages, done_tx)),
        #[cfg(test)]
        StorageEvent::TestBarrier { .. } => unreachable!("test barrier handled by writer"),
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
    _queue_budget: Arc<StorageQueueBudget>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut failed_message_records = 0_u64;
        let mut first_failure: Option<String> = None;
        let mut redaction_context = RedactionContextTracker::default();
        // Direction pumps reserve sequence before awaiting forwarding. Channel
        // arrival can therefore be reversed, even across writer batches.
        // Reservations remain owned here, so the existing byte budget also
        // bounds this reorder buffer. A second limit bounds its cardinality.
        let mut pending = BTreeMap::new();
        let mut next_seq = 0_u64;
        let mut arrival = 0_u64;
        let mut context_loss_reported = false;

        while let Ok(event) = rx.recv() {
            #[cfg(test)]
            let event = match event {
                StorageEvent::TestBarrier {
                    entered_tx,
                    release_rx,
                } => {
                    let _ = entered_tx.send(());
                    let _ = release_rx.recv();
                    continue;
                }
                event => event,
            };

            let mut messages = Vec::with_capacity(STORAGE_WRITE_BATCH_MESSAGES);
            let mut close = None;
            #[cfg(test)]
            let mut barrier = None;

            collect_storage_event(event, &mut messages, &mut close);
            while close.is_none() && messages.len() < STORAGE_WRITE_BATCH_MESSAGES {
                match rx.try_recv() {
                    #[cfg(test)]
                    Ok(StorageEvent::TestBarrier {
                        entered_tx,
                        release_rx,
                    }) => {
                        barrier = Some((entered_tx, release_rx));
                        break;
                    }
                    Ok(event) => collect_storage_event(event, &mut messages, &mut close),
                    Err(_) => break,
                }
            }

            for item in messages.drain(..) {
                pending.insert((item.0.seq, arrival), item);
                arrival = arrival.wrapping_add(1);
            }
            while let Some((&(seq, _), _)) = pending.first_key_value() {
                let pressure = pending.len() >= STORAGE_REORDER_MAX_MESSAGES;
                if seq > next_seq && close.is_none() && !pressure {
                    break;
                }
                if seq > next_seq && pressure && !context_loss_reported {
                    failed_message_records = failed_message_records.saturating_add(1);
                    first_failure.get_or_insert_with(|| {
                        "redaction sequence context was incomplete".to_string()
                    });
                    context_loss_reported = true;
                }
                let Some((_, item)) = pending.pop_first() else {
                    break;
                };
                next_seq = next_seq.max(seq.saturating_add(1));
                messages.push(item);
            }
            if !messages.is_empty() {
                for (message, _) in &mut messages {
                    let context = redaction_context.observe(
                        message.seq,
                        message.direction.as_db_str(),
                        &message.payload,
                    );
                    if redactor.is_active()
                        && redaction_context.has_lost_context()
                        && !context_loss_reported
                    {
                        failed_message_records = failed_message_records.saturating_add(1);
                        first_failure.get_or_insert_with(|| {
                            "redaction request context was incomplete".to_string()
                        });
                        context_loss_reported = true;
                    }
                    redactor.redact_with_context(&mut message.payload, context);
                }
                for batch in messages.chunks(STORAGE_WRITE_BATCH_MESSAGES) {
                    if let Err(err) =
                        store.write_messages(&session_id, batch.iter().map(|(message, _)| message))
                    {
                        warn!("failed to write MCP message batch: {err}");
                        failed_message_records = failed_message_records
                            .saturating_add(u64::try_from(batch.len()).unwrap_or(u64::MAX));
                        first_failure.get_or_insert_with(|| {
                            "failed to persist one or more MCP message records".to_string()
                        });
                    }
                }
                drop(messages);
            }

            #[cfg(test)]
            if let Some((entered_tx, release_rx)) = barrier {
                let _ = entered_tx.send(());
                let _ = release_rx.recv();
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
        .map_err(|_| anyhow!("storage writer stopped before the session could be finalized"))?;
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
/// or recording. Returns `false` if forwarding should stop (write/flush error,
/// unparseable frame, or a single frame over `MAX_FRAME_BYTES`).
///
/// The per-frame `MAX_FRAME_BYTES` check here, ahead of forwarding, is what
/// actually enforces the cap precisely. The caller's own check of the
/// leftover unterminated `buf` (after this returns) only catches a frame that
/// never completes; checking buffer size instead of `consumed` here would
/// misfire on a large frame immediately followed by the start of the next
/// one, since the pair can transiently exceed the cap in `buf` while neither
/// frame does on its own.
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

        if frame.consumed > MAX_FRAME_BYTES {
            // Forward-before-record still applies: a frame this large is a
            // protocol violation, not traffic to relay, so it must not reach
            // the other side any more than an unparseable one would.
            warn!(
                "{direction:?} stdio frame of {} bytes exceeded the {MAX_FRAME_BYTES} byte cap; stopping the pump",
                frame.consumed
            );
            return false;
        }

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
    let Some(reservation) = queue_budget.reserve_event(reserved_bytes) else {
        dropped_messages.fetch_add(1, Ordering::Relaxed);
        warn!("storage queue byte budget exhausted; dropped MCP message record");
        return;
    };

    match storage_tx.try_send(StorageEvent::Message {
        message,
        reservation,
    }) {
        Ok(()) => {}
        Err(TrySendError::Full(StorageEvent::Message { .. })) => {
            dropped_messages.fetch_add(1, Ordering::Relaxed);
            warn!("storage channel full; dropped MCP message record");
        }
        Err(TrySendError::Disconnected(StorageEvent::Message { .. })) => {
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
    use std::time::Duration;

    use mcptracer_protocol::Direction;
    use mcptracer_redact::{RedactionPolicy, Redactor};
    use mcptracer_storage::Store;
    use serde_json::json;

    use mcptracer_protocol::StdioFrame;

    use super::{
        drain_frames, finish_storage_writer, record_frame, redact_server_command,
        reserve_capture_stamp, spawn_storage_writer, try_record, CaptureStamp, McpMessage,
        StorageQueueBudget, MAX_FRAME_BYTES, STORAGE_QUEUE_MAX_BYTES,
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

    fn message_with_payload_bytes(seq: u64, payload_bytes: usize) -> McpMessage {
        McpMessage {
            payload_bytes,
            ..message(seq)
        }
    }

    /// Deliberately simple reference model: it owns its own counters and does
    /// not call the production reservation or channel helpers.
    struct QueueReference {
        max_events: usize,
        max_bytes: usize,
        events: usize,
        bytes: usize,
        dropped: u64,
        reservations: Vec<usize>,
        closed: bool,
    }

    impl QueueReference {
        fn new(max_events: usize, max_bytes: usize) -> Self {
            Self {
                max_events,
                max_bytes,
                events: 0,
                bytes: 0,
                dropped: 0,
                reservations: Vec::new(),
                closed: false,
            }
        }

        fn admit(&mut self, bytes: usize) -> bool {
            if self.closed || self.events == self.max_events || bytes > self.max_bytes - self.bytes
            {
                self.dropped += 1;
                return false;
            }
            self.events += 1;
            self.bytes += bytes;
            self.reservations.push(bytes);
            true
        }

        fn release_one(&mut self) {
            let bytes = self.reservations.pop().expect("owned reservation");
            self.events -= 1;
            self.bytes -= bytes;
        }

        fn close(&mut self) {
            while !self.reservations.is_empty() {
                self.release_one();
            }
            self.closed = true;
        }
    }

    fn pause_writer(tx: &mpsc::SyncSender<super::StorageEvent>) -> mpsc::Sender<()> {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        tx.send(super::StorageEvent::TestBarrier {
            entered_tx,
            release_rx,
        })
        .unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("writer must enter the deterministic barrier before the deadline");
        release_tx
    }

    fn finish_before_deadline(
        tx: mpsc::SyncSender<super::StorageEvent>,
        writer: std::thread::JoinHandle<()>,
        dropped: u64,
    ) -> anyhow::Result<()> {
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = finish_storage_writer(tx, writer, 1, dropped);
            let _ = done_tx.send(result);
        });
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("storage writer shutdown must complete before the deadline")
    }

    fn assert_saturation_case(max_events: usize, max_bytes: usize, attempts: usize) {
        let store = Store::open_in_memory().unwrap();
        let session_id = store
            .create_session("queue-test", "server", "stdio", 0)
            .unwrap();
        let (tx, rx) = mpsc::sync_channel(max_events);
        let budget = Arc::new(StorageQueueBudget::new(max_bytes));
        let writer = spawn_storage_writer(
            store,
            session_id,
            Redactor::new(RedactionPolicy::None),
            rx,
            Arc::clone(&budget),
        );
        let release_tx = pause_writer(&tx);
        let mut model = QueueReference::new(max_events, max_bytes);
        let dropped = AtomicU64::new(0);
        let reservation = 48 + super::STORAGE_EVENT_OVERHEAD_BYTES;

        for seq in 0..attempts as u64 {
            let expected_admitted = model.admit(reservation);
            try_record(&tx, message_with_payload_bytes(seq, 48), &dropped, &budget);
            assert_eq!(dropped.load(Ordering::Relaxed), model.dropped);
            assert_eq!(budget.queued_bytes(), model.bytes);
            assert!(model.bytes <= max_bytes, "reference budget exceeded");
            assert_eq!(
                expected_admitted,
                (seq as usize) < max_events && (seq as usize + 1) * reservation <= max_bytes
            );
        }
        assert!(model.dropped > 0, "the saturation guard must be exercised");

        release_tx.send(()).unwrap();
        model.close();
        model.close(); // repeated cleanup is idempotent in the independent oracle
        let result = finish_before_deadline(tx, writer, model.dropped).unwrap_err();
        assert!(result.to_string().contains("capture lost"), "{result}");
        assert_eq!(
            budget.queued_bytes(),
            0,
            "close must release every reservation"
        );
        assert_eq!(model.bytes, 0);
        assert_eq!(model.events, 0);
    }

    #[test]
    fn bounded_reference_sequence_matches_admit_release_drop_and_repeated_close() {
        let reservation = 48 + super::STORAGE_EVENT_OVERHEAD_BYTES;
        let (tx, rx) = mpsc::sync_channel(2);
        let budget = StorageQueueBudget::new(2 * reservation);
        let dropped = AtomicU64::new(0);
        let mut model = QueueReference::new(2, 2 * reservation);

        for seq in 0..2 {
            assert!(model.admit(reservation));
            try_record(&tx, message(seq), &dropped, &budget);
            assert_eq!(budget.queued_bytes(), model.bytes);
            assert_eq!(dropped.load(Ordering::Relaxed), model.dropped);
        }
        assert!(!model.admit(reservation));
        try_record(&tx, message(2), &dropped, &budget);
        assert_eq!(budget.queued_bytes(), model.bytes);
        assert_eq!(dropped.load(Ordering::Relaxed), model.dropped);

        // Releasing one owned queue event makes exactly one reservation
        // available again; a later admission consumes that room.
        drop(rx.try_recv().unwrap());
        model.release_one();
        assert_eq!(budget.queued_bytes(), model.bytes);
        assert!(model.admit(reservation));
        try_record(&tx, message(3), &dropped, &budget);
        assert_eq!(budget.queued_bytes(), model.bytes);
        assert_eq!(dropped.load(Ordering::Relaxed), model.dropped);

        drop(rx);
        model.close();
        model.close();
        assert_eq!(budget.queued_bytes(), 0);
        assert_eq!(model.bytes, 0);
        assert_eq!(model.events, 0);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn writer_barrier_proves_channel_slot_saturation_and_cleanup() {
        // Keep the byte ceiling high so only the event-slot bound is reached.
        assert_saturation_case(2, 16 * 1024, 4);
    }

    #[test]
    fn writer_barrier_proves_byte_budget_saturation_before_slot_capacity() {
        // Eight slots are available, but only two message reservations fit.
        assert_saturation_case(8, 2 * (48 + super::STORAGE_EVENT_OVERHEAD_BYTES), 4);
    }

    #[test]
    fn writer_barrier_control_completes_without_pressure() {
        let store = Store::open_in_memory().unwrap();
        let session_id = store
            .create_session("queue-control", "server", "stdio", 0)
            .unwrap();
        let (tx, rx) = mpsc::sync_channel(4);
        let budget = Arc::new(StorageQueueBudget::new(16 * 1024));
        let writer = spawn_storage_writer(
            store,
            session_id,
            Redactor::new(RedactionPolicy::None),
            rx,
            Arc::clone(&budget),
        );
        let release_tx = pause_writer(&tx);
        let dropped = AtomicU64::new(0);
        let mut model = QueueReference::new(4, 16 * 1024);
        for seq in 0..2 {
            assert!(model.admit(48 + super::STORAGE_EVENT_OVERHEAD_BYTES));
            try_record(&tx, message(seq), &dropped, &budget);
        }
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert_eq!(budget.queued_bytes(), model.bytes);
        release_tx.send(()).unwrap();
        model.close();
        finish_before_deadline(tx, writer, model.dropped).unwrap();
        assert_eq!(budget.queued_bytes(), 0);
    }

    #[test]
    fn dropping_a_paused_writer_queue_reclaims_queued_reservations() {
        let (tx, rx) = mpsc::sync_channel(2);
        let dropped = AtomicU64::new(0);
        let budget = StorageQueueBudget::new(4096);
        try_record(&tx, message(0), &dropped, &budget);
        try_record(&tx, message(1), &dropped, &budget);
        assert_eq!(
            budget.queued_bytes(),
            2 * (48 + super::STORAGE_EVENT_OVERHEAD_BYTES)
        );
        drop(rx);
        assert_eq!(
            budget.queued_bytes(),
            0,
            "channel teardown drops owned reservations"
        );
        // A post-disconnect attempt is rejected and has no reservation left to
        // release a second time.
        try_record(&tx, message(2), &dropped, &budget);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(budget.queued_bytes(), 0);
    }

    #[test]
    fn disconnected_writer_returns_its_reservation_once() {
        let (tx, rx) = mpsc::sync_channel(1);
        let dropped = AtomicU64::new(0);
        let budget = StorageQueueBudget::new(4096);
        drop(rx);
        try_record(&tx, message(0), &dropped, &budget);
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(budget.queued_bytes(), 0);
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
        let full_reservation = budget
            .reserve_event(STORAGE_QUEUE_MAX_BYTES)
            .expect("the exact queue cap must be reservable");
        assert_eq!(budget.queued_bytes(), STORAGE_QUEUE_MAX_BYTES);
        assert!(
            budget.reserve_event(1).is_none(),
            "a budget reserved to its cap must refuse even a single further byte"
        );

        drop(full_reservation);
        assert_eq!(budget.queued_bytes(), 0);

        // Just under, then the byte that lands exactly on the cap.
        let almost_full = budget
            .reserve_event(STORAGE_QUEUE_MAX_BYTES - 1)
            .expect("the budget must admit all but one byte");
        let final_byte = budget
            .reserve_event(1)
            .expect("the final byte up to the cap must still fit");
        assert!(budget.reserve_event(1).is_none());
        drop((almost_full, final_byte));
        assert_eq!(budget.queued_bytes(), 0);

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

    /// One complete, newline-terminated stdio frame of exactly `total_len`
    /// bytes (including the trailing `\n`). The filler byte is never `\n` or
    /// `\r`, so this is the only frame boundary in the buffer.
    fn frame_bytes_of_len(total_len: usize) -> Vec<u8> {
        let mut bytes = vec![b'a'; total_len - 1];
        bytes.push(b'\n');
        bytes
    }

    #[tokio::test]
    async fn drain_frames_admits_exactly_the_cap_and_one_byte_under_it() {
        // The pass case: a single frame at, or just under, the real
        // MAX_FRAME_BYTES must still be forwarded and drained, not rejected
        // as oversized. This is the boundary the inventory flagged as
        // untested everywhere ("exactly-at and just-under -- the pass case").
        for total_len in [MAX_FRAME_BYTES, MAX_FRAME_BYTES - 1] {
            let mut buf = frame_bytes_of_len(total_len);
            let mut writer: Vec<u8> = Vec::new();
            let (tx, _rx) = mpsc::sync_channel(4);
            let seq = AtomicU64::new(0);
            let dropped = AtomicU64::new(0);
            let budget = StorageQueueBudget::new(STORAGE_QUEUE_MAX_BYTES);

            let kept_pumping = drain_frames(
                &mut buf,
                Direction::ClientToServer,
                &mut writer,
                &tx,
                &seq,
                &dropped,
                &budget,
            )
            .await;

            assert!(kept_pumping, "a {total_len}-byte frame must not be capped");
            assert!(buf.is_empty(), "the complete frame must be fully drained");
            assert_eq!(
                writer.len(),
                total_len,
                "the whole frame must be forwarded, cap or no cap"
            );
        }
    }

    #[tokio::test]
    async fn drain_frames_refuses_a_single_frame_one_byte_past_the_cap() {
        // The reject case at the true boundary: earlier coverage only proved
        // an unterminated frame that never completes gets capped (9 MiB of
        // bytes with no newline at all). A frame that legitimately completes
        // with a newline just one byte over MAX_FRAME_BYTES must be refused
        // too, and -- forward-before-record notwithstanding -- must not reach
        // the writer, the same as an unparseable frame never does.
        let mut buf = frame_bytes_of_len(MAX_FRAME_BYTES + 1);
        let mut writer: Vec<u8> = Vec::new();
        let (tx, _rx) = mpsc::sync_channel(4);
        let seq = AtomicU64::new(0);
        let dropped = AtomicU64::new(0);
        let budget = StorageQueueBudget::new(STORAGE_QUEUE_MAX_BYTES);

        let kept_pumping = drain_frames(
            &mut buf,
            Direction::ClientToServer,
            &mut writer,
            &tx,
            &seq,
            &dropped,
            &budget,
        )
        .await;

        assert!(
            !kept_pumping,
            "a frame one byte over the cap must stop the pump"
        );
        assert!(
            writer.is_empty(),
            "an oversized frame must not be forwarded, matching an unparseable one"
        );
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
        let dir = std::env::temp_dir().join(format!(
            "mcptracer-writer-failure-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("sessions.db");
        let store = Store::open(&db_path).unwrap();
        let session_id = store.create_session("test", "server", "stdio", 0).unwrap();
        let session_id_for_check = session_id.clone();
        let (tx, rx) = mpsc::sync_channel(4);
        let budget = Arc::new(StorageQueueBudget::new(4 * 1024));
        let writer = spawn_storage_writer(
            store,
            session_id,
            Redactor::new(RedactionPolicy::None),
            rx,
            Arc::clone(&budget),
        );
        let release_tx = pause_writer(&tx);
        let dropped = AtomicU64::new(0);
        let mut model = QueueReference::new(4, 4 * 1024);
        for _ in 0..2 {
            assert!(model.admit(48 + super::STORAGE_EVENT_OVERHEAD_BYTES));
            try_record(&tx, message(0), &dropped, &budget);
        }
        assert_eq!(budget.queued_bytes(), model.bytes);
        release_tx.send(()).unwrap();
        model.close();
        model.close();

        let err = finish_before_deadline(tx, writer, model.dropped).unwrap_err();
        assert!(err
            .to_string()
            .contains("failed to persist one or more MCP message records"));
        assert_eq!(budget.queued_bytes(), 0);

        let store = Store::open(&db_path).unwrap();
        let summary = store.get_session_summary(&session_id_for_check).unwrap();
        assert_eq!(
            summary.total_messages, 0,
            "the failed transaction is atomic"
        );
        assert_eq!(
            summary.dropped_messages, 2,
            "failed records are persisted as drops"
        );
        let report = mcptracer_model::validate_session(
            &store.get_messages(&session_id_for_check).unwrap(),
            summary.dropped_messages as u64,
        );
        assert!(
            !report.is_healthy(),
            "a session with persistence loss must fail health validation"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schema_context_survives_reversed_arrival_across_writer_batches() {
        let dir = std::env::temp_dir().join(format!(
            "mcptracer-schema-order-{}-{}",
            std::process::id(),
            super::now_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("capture.db");
        let store = Store::open(&db).unwrap();
        let id = store.create_session("test", "dummy", "stdio", 0).unwrap();
        store.set_redaction_policy(&id, "default", &[]).unwrap();
        let (tx, rx) = mpsc::sync_channel(4);
        let budget = Arc::new(StorageQueueBudget::new(8192));
        let writer = spawn_storage_writer(
            store,
            id.clone(),
            Redactor::new(RedactionPolicy::Default),
            rx,
            Arc::clone(&budget),
        );
        let dropped = AtomicU64::new(0);
        let response = McpMessage {
            seq: 1,
            direction: Direction::ServerToClient,
            timestamp_ns: 1,
            payload: json!({"jsonrpc":"2.0","id":1,"result":{"tools":[{
                "name":"dummy","inputSchema":{"type":"object","properties":{"api_key":{"type":"string"}},"password":"dummy-runtime-value"}
            }]}}),
            payload_bytes: 256,
        };
        // The response is consumed in an earlier writer batch while the
        // direction pump has not yet enqueued its lower-sequence request.
        try_record(&tx, response, &dropped, &budget);
        let release = pause_writer(&tx);
        assert!(
            budget.queued_bytes() > 0,
            "reorder buffer must own the reservation"
        );
        try_record(
            &tx,
            McpMessage {
                payload: json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
                ..message(0)
            },
            &dropped,
            &budget,
        );
        release.send(()).unwrap();
        finish_before_deadline(tx, writer, 0).unwrap();
        assert_eq!(budget.queued_bytes(), 0);
        let store = Store::open(&db).unwrap();
        let messages = store.get_messages(&id).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&messages[1].payload).unwrap();
        assert_eq!(
            payload["result"]["tools"][0]["inputSchema"]["properties"]["api_key"],
            json!({"type":"string"})
        );
        assert!(!payload.to_string().contains("dummy-runtime-value"));
        assert!(crate::session_health::inspect_session(&store, &id)
            .unwrap()
            .report
            .is_healthy());
        let artifact = store
            .export_mtrace_document(&id, Default::default())
            .unwrap();
        mcptracer_storage::mtrace::validate(&artifact).unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
