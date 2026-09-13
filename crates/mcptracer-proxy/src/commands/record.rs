use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use mcptracer_protocol::Direction;
use mcptracer_redact::{RedactionPolicy, Redactor};
use mcptracer_storage::Store;
use tokio::io::AsyncReadExt;
use tokio::process::Child;

use crate::session_writer::{
    drain_frames, finish_storage_writer, now_ns, redact_server_command, spawn_server_process,
    spawn_storage_writer, StorageEvent, StorageQueueBudget, STORAGE_QUEUE_MAX_BYTES,
};
use crate::shutdown::{
    listen_for_stop_requests, next_stop_request, stop_was_requested, StopRequests,
};

/// How long the server->client pump keeps draining after the client closes
/// stdin. A well-behaved MCP server treats stdin EOF as "shut down" and may
/// still flush a final response or two on the way out; recording those is the
/// difference between a complete capture and a session that ends mid-exchange.
/// Matched to `replay`'s default `--request-timeout`, so a final in-flight
/// tool call has the same budget to answer that it would have had mid-session
/// — but still bounded, because a server that ignores stdin EOF must not be
/// able to keep the recorder alive forever.
const CLIENT_EOF_DRAIN_GRACE: Duration = Duration::from_secs(30);

/// How long to wait for the server process to be reaped once forwarding has
/// stopped, before killing it. Past this point nothing is relaying its output
/// anywhere, so leaving it alive only orphans a process the client can no
/// longer reach.
const SERVER_REAP_GRACE: Duration = Duration::from_secs(5);

/// Which side ended the proxying loop first. The recorder has two independent
/// pumps and a stop-request listener, and all three have to be able to stop the
/// other two: before this existed, a server that exited left the client->server
/// pump blocked on stdin forever, so the recorder never exited and never closed
/// its own stdout — turning a server crash into a silent hang for the MCP client.
enum ProxyOutcome {
    ClientClosed(Result<()>),
    ServerClosed(Result<()>),
    Stopped,
}

/// How forwarding ended, once any trailing server output has been drained.
struct PumpsOutcome {
    result: Result<()>,
    /// An operator stop request ended forwarding or cut the drain short.
    stopped: bool,
}

#[derive(Args)]
pub struct RecordArgs {
    #[arg(long, default_value = "unknown")]
    pub client: String,

    /// Redaction policy applied to stored payloads: `none` or `default`.
    /// Forwarded bytes are never modified.
    #[arg(long, default_value = "none")]
    pub redact: String,

    /// Extra JSON field names to mask, comma-separated. Requires
    /// `--redact default` and is stored as the `custom` policy.
    #[arg(long, value_delimiter = ',')]
    pub redact_keys: Vec<String>,

    #[arg(trailing_var_arg = true, required = true)]
    pub server_args: Vec<String>,
}

pub async fn run(args: RecordArgs, db_path: PathBuf) -> Result<()> {
    if args.server_args.is_empty() {
        return Err(anyhow!("missing MCP server command after --"));
    }
    // Listen before the server or the session exist, so a stop request from
    // here on finalizes whatever has been recorded rather than abandoning it.
    let mut stop = listen_for_stop_requests();
    let server_command = redact_server_command(&args.server_args);

    let policy = RedactionPolicy::from_cli(&args.redact, &args.redact_keys)
        .map_err(|error| anyhow!(error))?;
    let redactor = Redactor::new(policy.clone());

    let mut server_process = spawn_server_process(&args.server_args)?;
    let server_stdin = server_process
        .stdin
        .take()
        .ok_or_else(|| anyhow!("failed to open server stdin"))?;
    let server_stdout = server_process
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to open server stdout"))?;

    let store = Store::open(&db_path)?;
    let session_id = store.create_session(&args.client, &server_command, "stdio", now_ns())?;
    if redactor.is_active() {
        store.set_redaction_policy(&session_id, policy.as_str(), policy.custom_keys())?;
    }
    let (storage_tx, storage_rx) = mpsc::sync_channel::<StorageEvent>(4096);
    let queue_budget = Arc::new(StorageQueueBudget::new(STORAGE_QUEUE_MAX_BYTES));
    let storage_handle = spawn_storage_writer(
        store,
        session_id.clone(),
        redactor,
        storage_rx,
        Arc::clone(&queue_budget),
    );

    eprintln!(
        "[mcptracer] recording session {} -> {}",
        session_id,
        db_path.display()
    );

    let seq = Arc::new(AtomicU64::new(0));
    let dropped_messages = Arc::new(AtomicU64::new(0));
    let c2s = proxy_client_to_server(
        server_stdin,
        storage_tx.clone(),
        Arc::clone(&seq),
        Arc::clone(&dropped_messages),
        Arc::clone(&queue_budget),
    );
    let s2c = proxy_server_to_client(
        server_stdout,
        storage_tx.clone(),
        Arc::clone(&seq),
        Arc::clone(&dropped_messages),
        Arc::clone(&queue_budget),
    );

    // The pumps own every `storage_tx` clone but the one kept here, and are
    // consumed by this call, so the session can be finalized right after it.
    let pumps = coordinate_pumps(c2s, s2c, &mut stop, CLIENT_EOF_DRAIN_GRACE).await;

    // A stop request may reach the server as well as the recorder - a
    // terminal Ctrl-C signals the whole process group - and the server's exit
    // can then win the race to end forwarding. That is still an operator stop,
    // so it is judged by whether one arrived, not by which future won.
    if pumps.stopped || stop_was_requested(&stop) {
        eprintln!("[mcptracer] stop requested; finalizing session {session_id}");
        // Count that request as seen, so only a further one cuts the reap short.
        stop.borrow_and_update();
    }

    // Finalize before reaping. Waiting on a stubborn server can take the whole
    // grace period, and a client that follows SIGTERM with SIGKILL would kill
    // the recorder mid-wait and leave the session unclosed. Nothing the server
    // does from here can reach the recording: the pumps are gone.
    let storage_result = finish_storage_writer(
        storage_tx,
        storage_handle,
        now_ns(),
        dropped_messages.load(Ordering::Relaxed),
    );
    let process_result = reap_server(&mut server_process, SERVER_REAP_GRACE, &mut stop).await;

    pumps.result?;
    storage_result?;

    // A server stopped on the operator's behalf - by us, or by the same signal
    // that stopped us - has no meaningful exit status. Only a server that
    // exited on its own has its status checked.
    let stopped = pumps.stopped || stop_was_requested(&stop);
    if let Some(status) = process_result.context("failed to wait for MCP server process")? {
        if !status.success() && !stopped {
            return Err(anyhow!("MCP server exited unsuccessfully: {status}"));
        }
    }

    eprintln!("[mcptracer] session {} ended", session_id);
    Ok(())
}

/// Run both forwarding pumps until one ends or an operator stop is requested,
/// then drain the server's trailing output if it was the client that left.
///
/// Generic over the pumps so every branch - including both timeouts, which
/// used to be reachable only by waiting them out in real time - can be driven
/// deterministically in tests.
async fn coordinate_pumps<C, S>(
    client_to_server: C,
    server_to_client: S,
    stop: &mut StopRequests,
    drain_grace: Duration,
) -> PumpsOutcome
where
    C: Future<Output = Result<()>>,
    S: Future<Output = Result<()>>,
{
    let mut c2s = pin!(client_to_server);
    let mut s2c = pin!(server_to_client);

    let outcome = tokio::select! {
        result = &mut c2s => ProxyOutcome::ClientClosed(result),
        result = &mut s2c => ProxyOutcome::ServerClosed(result),
        () = next_stop_request(stop) => ProxyOutcome::Stopped,
    };

    match outcome {
        ProxyOutcome::ClientClosed(result) => {
            // `c2s` has returned, so it has already dropped the server's stdin
            // handle: the server sees EOF and can shut down. Keep recording
            // whatever it emits on the way out - but stay stoppable, or a
            // server that never closes its output holds the operator hostage
            // for the whole grace period.
            let drain = tokio::select! {
                drained = tokio::time::timeout(drain_grace, &mut s2c) => Some(drained),
                () = next_stop_request(stop) => None,
            };
            match drain {
                Some(Ok(drained)) => PumpsOutcome {
                    result: result.and(drained),
                    stopped: false,
                },
                Some(Err(_elapsed)) => {
                    eprintln!(
                        "[mcptracer] MCP server did not close its output within \
                         {drain_grace:?} of the client disconnecting; stopping capture"
                    );
                    PumpsOutcome {
                        result,
                        stopped: false,
                    }
                }
                None => PumpsOutcome {
                    result,
                    stopped: true,
                },
            }
        }
        // The server is gone. Abandoning `c2s` here is the whole point: it
        // would otherwise sit on client stdin forever. Returning also closes
        // this process's stdout, which is how the MCP client finally observes
        // that its server has exited.
        ProxyOutcome::ServerClosed(result) => PumpsOutcome {
            result,
            stopped: false,
        },
        ProxyOutcome::Stopped => PumpsOutcome {
            result: Ok(()),
            stopped: true,
        },
    }
}

/// Wait up to `grace` for the server to exit on its own, then terminate it.
///
/// A further stop request skips the rest of the wait: an operator who asks
/// twice should not have to sit out the grace period. Returns the observed exit
/// status, or `None` when the process had to be terminated.
async fn reap_server(
    server: &mut Child,
    grace: Duration,
    stop: &mut StopRequests,
) -> std::io::Result<Option<ExitStatus>> {
    let waited = tokio::select! {
        waited = tokio::time::timeout(grace, server.wait()) => Some(waited),
        () = next_stop_request(stop) => None,
    };
    match waited {
        Some(Ok(status)) => return status.map(Some),
        Some(Err(_elapsed)) => {
            eprintln!("[mcptracer] MCP server did not exit within {grace:?}; terminating it");
        }
        None => eprintln!("[mcptracer] stop requested again; terminating the MCP server now"),
    }
    match server.kill().await {
        Ok(()) => Ok(None),
        // The server can exit on its own between the wait ending and the kill
        // landing, in which case there is nothing left to kill.
        Err(error) => match server.try_wait()? {
            Some(status) => Ok(Some(status)),
            None => Err(error),
        },
    }
}

async fn proxy_client_to_server(
    mut server_stdin: tokio::process::ChildStdin,
    storage_tx: SyncSender<StorageEvent>,
    seq: Arc<AtomicU64>,
    dropped_messages: Arc<AtomicU64>,
    queue_budget: Arc<StorageQueueBudget>,
) -> Result<()> {
    let mut proxy_stdin = tokio::io::stdin();
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut tmp = [0_u8; 4096];

    loop {
        match proxy_stdin.read(&mut tmp).await {
            Ok(0) => return Ok(()),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if !drain_frames(
                    &mut buf,
                    Direction::ClientToServer,
                    &mut server_stdin,
                    &storage_tx,
                    &seq,
                    &dropped_messages,
                    &queue_budget,
                )
                .await
                {
                    return Err(anyhow!("client-to-server forwarding stopped"));
                }
                if buf.len() > crate::session_writer::MAX_FRAME_BYTES {
                    // Returning Ok here would leave the session silently
                    // half-open: this direction stops relaying while the other
                    // keeps running, so the client waits forever on a reply
                    // that can no longer arrive. Fail loudly instead.
                    return Err(anyhow!(
                        "client frame exceeded {} bytes without a newline; stopping client->server pump",
                        crate::session_writer::MAX_FRAME_BYTES
                    ));
                }
            }
            Err(err) => {
                return Err(anyhow!("failed to read client stdin: {err}"));
            }
        }
    }
}

async fn proxy_server_to_client(
    mut server_stdout: tokio::process::ChildStdout,
    storage_tx: SyncSender<StorageEvent>,
    seq: Arc<AtomicU64>,
    dropped_messages: Arc<AtomicU64>,
    queue_budget: Arc<StorageQueueBudget>,
) -> Result<()> {
    let mut proxy_stdout = tokio::io::stdout();
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut tmp = [0_u8; 4096];

    loop {
        match server_stdout.read(&mut tmp).await {
            Ok(0) => return Ok(()),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if !drain_frames(
                    &mut buf,
                    Direction::ServerToClient,
                    &mut proxy_stdout,
                    &storage_tx,
                    &seq,
                    &dropped_messages,
                    &queue_budget,
                )
                .await
                {
                    return Err(anyhow!("server-to-client forwarding stopped"));
                }
                if buf.len() > crate::session_writer::MAX_FRAME_BYTES {
                    // Returning Ok here would leave the session silently
                    // half-open: this direction stops relaying while the other
                    // keeps running, so the client waits forever on a reply
                    // that can no longer arrive. Fail loudly instead.
                    return Err(anyhow!(
                        "server frame exceeded {} bytes without a newline; stopping server->client pump",
                        crate::session_writer::MAX_FRAME_BYTES
                    ));
                }
            }
            Err(err) => {
                return Err(anyhow!("failed to read server stdout: {err}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::{pending, Future};
    use std::process::Stdio;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use tokio::process::{Child, Command};
    use tokio::sync::watch;
    use tokio::time::Instant;

    use super::{coordinate_pumps, reap_server};

    const DRAIN_GRACE: Duration = Duration::from_secs(30);

    fn never() -> impl Future<Output = Result<()>> {
        pending()
    }

    async fn done() -> Result<()> {
        Ok(())
    }

    async fn after(delay: Duration) -> Result<()> {
        tokio::time::sleep(delay).await;
        Ok(())
    }

    fn request_stop(requests: &watch::Sender<u64>) {
        requests.send_modify(|count| *count += 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_closes_first_ends_forwarding_without_waiting() {
        let (_requests, mut stop) = watch::channel(0_u64);
        let start = Instant::now();

        let outcome = coordinate_pumps(never(), done(), &mut stop, DRAIN_GRACE).await;

        assert!(outcome.result.is_ok());
        assert!(!outcome.stopped);
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "a closed server must not wait on the client"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn trailing_output_inside_the_grace_period_is_still_captured() {
        let (_requests, mut stop) = watch::channel(0_u64);
        let start = Instant::now();

        let outcome = coordinate_pumps(
            done(),
            after(Duration::from_secs(10)),
            &mut stop,
            DRAIN_GRACE,
        )
        .await;

        assert!(outcome.result.is_ok());
        assert!(!outcome.stopped);
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(10),
            "the drain must wait for the server's output, and no longer"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_drain_gives_up_exactly_at_the_grace_period() {
        let (_requests, mut stop) = watch::channel(0_u64);
        let start = Instant::now();

        let outcome = coordinate_pumps(done(), never(), &mut stop, DRAIN_GRACE).await;

        // Reaching the timeout is a normal end of capture, not a failure.
        assert!(outcome.result.is_ok());
        assert!(!outcome.stopped);
        assert_eq!(start.elapsed(), DRAIN_GRACE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_request_ends_forwarding() {
        let (requests, mut stop) = watch::channel(0_u64);
        request_stop(&requests);
        let start = Instant::now();

        let outcome = coordinate_pumps(never(), never(), &mut stop, DRAIN_GRACE).await;

        assert!(outcome.result.is_ok());
        assert!(outcome.stopped);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_request_cuts_the_drain_short() {
        // Before stop requests were shared across stages, the Ctrl-C listener
        // was consumed by the first `select!` and this wait could not be
        // interrupted at all: the operator sat out the whole grace period.
        let (requests, mut stop) = watch::channel(0_u64);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            request_stop(&requests);
        });
        let start = Instant::now();

        let outcome = coordinate_pumps(done(), never(), &mut stop, DRAIN_GRACE).await;

        assert!(outcome.result.is_ok());
        assert!(outcome.stopped);
        assert_eq!(start.elapsed(), Duration::from_secs(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_listener_that_failed_to_register_never_stops_the_recording() {
        // Awaiting `tokio::signal::ctrl_c()` directly yields `Err` at once when
        // no handler can be installed, which ended the recording immediately.
        let (requests, mut stop) = watch::channel(0_u64);
        drop(requests);
        let start = Instant::now();

        let outcome = coordinate_pumps(
            never(),
            after(Duration::from_secs(5)),
            &mut stop,
            DRAIN_GRACE,
        )
        .await;

        assert!(!outcome.stopped);
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_side_error_is_reported_even_when_the_server_drains_cleanly() {
        let (_requests, mut stop) = watch::channel(0_u64);

        // The server finishes later, so the client side is unambiguously first:
        // pumps that finish together race, and a server that closes first
        // abandons the client pump by design.
        let outcome = coordinate_pumps(
            async { Err(anyhow!("client pump failed")) },
            after(Duration::from_secs(1)),
            &mut stop,
            DRAIN_GRACE,
        )
        .await;

        let error = outcome
            .result
            .expect_err("the client error must not be lost");
        assert!(error.to_string().contains("client pump failed"), "{error}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_side_error_during_the_drain_is_reported() {
        let (_requests, mut stop) = watch::channel(0_u64);

        let outcome = coordinate_pumps(
            done(),
            async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Err(anyhow!("server pump failed"))
            },
            &mut stop,
            DRAIN_GRACE,
        )
        .await;

        let error = outcome
            .result
            .expect_err("the server error must not be lost");
        assert!(error.to_string().contains("server pump failed"), "{error}");
    }

    // The reap tests use real processes and real time. Paused time would
    // auto-advance past the timeout while the runtime waits on the OS for the
    // child, firing the timeout whether or not the child had exited.

    fn exits_immediately() -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("cmd");
            command.args(["/C", "exit", "0"]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sh");
            command.args(["-c", "exit 0"]);
            command
        }
    }

    fn runs_until_killed() -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("ping");
            command.args(["-n", "300", "127.0.0.1"]);
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sleep");
            command.arg("300");
            command
        }
    }

    fn spawn(mut command: Command) -> Child {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn test process")
    }

    #[tokio::test]
    async fn reap_returns_the_status_of_a_server_that_exits_in_time() {
        let (_requests, mut stop) = watch::channel(0_u64);
        let mut server = spawn(exits_immediately());

        let status = reap_server(&mut server, Duration::from_secs(60), &mut stop)
            .await
            .expect("reap failed")
            .expect("a server that exits in time must not be terminated");

        assert!(status.success());
    }

    #[tokio::test]
    async fn reap_terminates_a_server_that_outlives_the_grace_period() {
        let (_requests, mut stop) = watch::channel(0_u64);
        let mut server = spawn(runs_until_killed());
        let grace = Duration::from_millis(300);
        let start = std::time::Instant::now();

        let status = reap_server(&mut server, grace, &mut stop)
            .await
            .expect("reap failed");

        assert!(status.is_none(), "an overdue server must be terminated");
        assert!(start.elapsed() >= grace, "it must wait out the grace first");
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "termination must not hang"
        );
        assert!(
            server.try_wait().expect("try_wait failed").is_some(),
            "the server process must actually be gone"
        );
    }

    #[tokio::test]
    async fn a_second_stop_request_skips_the_rest_of_the_grace_period() {
        let (requests, mut stop) = watch::channel(0_u64);
        let mut server = spawn(runs_until_killed());
        request_stop(&requests);
        let start = std::time::Instant::now();

        let status = reap_server(&mut server, Duration::from_secs(300), &mut stop)
            .await
            .expect("reap failed");

        assert!(status.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "a stop request must not wait for the 300s grace"
        );
        assert!(server.try_wait().expect("try_wait failed").is_some());
    }
}
