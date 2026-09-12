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
/// pumps and a signal handler, and all three have to be able to stop the other
/// two: before this existed, a server that exited left the client->server pump
/// blocked on stdin forever, so the recorder never exited and never closed its
/// own stdout — turning a server crash into a silent hang for the MCP client.
enum ProxyOutcome {
    ClientClosed(Result<()>),
    ServerClosed(Result<()>),
    Interrupted,
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

    // Scoped so both pump futures — and the `storage_tx` clones they own —
    // are dropped before the session is finalized below.
    let (pump_result, interrupted) = {
        let mut c2s = pin!(c2s);
        let mut s2c = pin!(s2c);

        let outcome = tokio::select! {
            result = &mut c2s => ProxyOutcome::ClientClosed(result),
            result = &mut s2c => ProxyOutcome::ServerClosed(result),
            _ = tokio::signal::ctrl_c() => ProxyOutcome::Interrupted,
        };

        match outcome {
            ProxyOutcome::ClientClosed(result) => {
                // `c2s` has returned, so it has already dropped the server's
                // stdin handle: the server sees EOF and can shut down. Keep
                // recording whatever it emits on the way out.
                let drained = match tokio::time::timeout(CLIENT_EOF_DRAIN_GRACE, &mut s2c).await {
                    Ok(drained) => drained,
                    Err(_) => {
                        tracing::warn!(
                            "server did not close stdout within {}s of client EOF; \
                             stopping capture",
                            CLIENT_EOF_DRAIN_GRACE.as_secs()
                        );
                        Ok(())
                    }
                };
                (result.and(drained), false)
            }
            // The server is gone. Abandoning `c2s` here is the whole point:
            // it would otherwise sit on client stdin forever. Returning also
            // closes this process's stdout, which is how the MCP client
            // finally observes that its server has exited.
            ProxyOutcome::ServerClosed(result) => (result, false),
            ProxyOutcome::Interrupted => {
                eprintln!("[mcptracer] interrupted; finalizing session {session_id}");
                (Ok(()), true)
            }
        }
    };

    let process_result = reap_server(&mut server_process).await;
    let storage_result = finish_storage_writer(
        storage_tx,
        storage_handle,
        now_ns(),
        dropped_messages.load(Ordering::Relaxed),
    );

    pump_result?;
    storage_result?;

    // A server we terminated ourselves has no meaningful exit status, so only
    // a server that exited on its own gets its status checked.
    if let Some(status) = process_result.context("failed to wait for MCP server process")? {
        if !status.success() && !interrupted {
            return Err(anyhow!("MCP server exited unsuccessfully: {status}"));
        }
    }

    eprintln!("[mcptracer] session {} ended", session_id);
    Ok(())
}

/// Wait briefly for the server to exit on its own, then kill it. Returns the
/// observed exit status, or `None` if the process had to be terminated.
async fn reap_server(server_process: &mut Child) -> std::io::Result<Option<ExitStatus>> {
    match tokio::time::timeout(SERVER_REAP_GRACE, server_process.wait()).await {
        Ok(status) => status.map(Some),
        Err(_) => {
            server_process.kill().await?;
            server_process.wait().await?;
            Ok(None)
        }
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
