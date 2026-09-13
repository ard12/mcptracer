//! Operator stop requests, shared by the long-running commands.
//!
//! `record` has to notice a stop request at every stage of shutting down:
//! while forwarding, while draining the server's trailing output, and while
//! waiting for the server to exit. `tokio::signal::ctrl_c()` is a single-shot
//! future, so the first stage consumed it and the later stages had nothing
//! listening. Tokio's handler stayed installed, so the platform's default
//! "terminate the process" behavior was gone as well: a second Ctrl-C was
//! silently swallowed for the whole 30-second drain or 5-second reap.
//!
//! Requests are therefore counted on a watch channel that any stage can wait
//! on, and a request that arrives between stages is not lost.

use std::future::pending;

use tokio::sync::watch;

/// Observes operator stop requests. The value is the number received so far.
pub type StopRequests = watch::Receiver<u64>;

/// Start forwarding operator stop signals onto a watch channel: Ctrl-C and
/// Ctrl-Break on Windows, SIGINT and SIGTERM elsewhere.
///
/// SIGTERM is included because it is how MCP clients and process managers ask
/// a server to stop. Left at its default it ends the process at once and
/// abandons the recording unclosed.
pub fn listen_for_stop_requests() -> StopRequests {
    let (requests, receiver) = watch::channel(0_u64);
    tokio::spawn(forward_stop_signals(requests));
    receiver
}

/// Resolve on the next stop request not yet observed through `requests`.
///
/// If no signal handler could be installed this never resolves. Awaiting
/// `tokio::signal::ctrl_c()` directly treats a failed registration as a stop
/// request, because it yields `Err` immediately, which would end a recording
/// the moment it started - most plausibly for a process an MCP client
/// launched without a console. With no handler installed, the platform's
/// default behavior still stops the process.
pub async fn next_stop_request(requests: &mut StopRequests) {
    if requests.changed().await.is_err() {
        pending::<()>().await;
    }
}

/// Whether any stop request has arrived, observed or not.
pub fn stop_was_requested(requests: &StopRequests) -> bool {
    *requests.borrow() > 0
}

fn registered<T>(name: &str, registration: std::io::Result<T>) -> Option<T> {
    match registration {
        Ok(listener) => Some(listener),
        Err(error) => {
            tracing::warn!("could not listen for {name} ({error}); continuing without it");
            None
        }
    }
}

#[cfg(unix)]
async fn forward_stop_signals(requests: watch::Sender<u64>) {
    use tokio::signal::unix::{signal, Signal, SignalKind};

    async fn next(listener: &mut Option<Signal>) -> Option<()> {
        match listener {
            Some(listener) => listener.recv().await,
            None => pending().await,
        }
    }

    let mut interrupt = registered("SIGINT", signal(SignalKind::interrupt()));
    let mut terminate = registered("SIGTERM", signal(SignalKind::terminate()));
    if interrupt.is_none() && terminate.is_none() {
        return;
    }
    loop {
        tokio::select! {
            Some(()) = next(&mut interrupt) => {}
            Some(()) = next(&mut terminate) => {}
            else => return,
        }
        requests.send_modify(|count| *count += 1);
    }
}

#[cfg(windows)]
async fn forward_stop_signals(requests: watch::Sender<u64>) {
    use tokio::signal::windows::{ctrl_break, ctrl_c, CtrlBreak, CtrlC};

    async fn next_interrupt(listener: &mut Option<CtrlC>) -> Option<()> {
        match listener {
            Some(listener) => listener.recv().await,
            None => pending().await,
        }
    }

    async fn next_break(listener: &mut Option<CtrlBreak>) -> Option<()> {
        match listener {
            Some(listener) => listener.recv().await,
            None => pending().await,
        }
    }

    let mut interrupt = registered("Ctrl-C", ctrl_c());
    let mut break_signal = registered("Ctrl-Break", ctrl_break());
    if interrupt.is_none() && break_signal.is_none() {
        return;
    }
    loop {
        tokio::select! {
            Some(()) = next_interrupt(&mut interrupt) => {}
            Some(()) = next_break(&mut break_signal) => {}
            else => return,
        }
        requests.send_modify(|count| *count += 1);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::watch;

    use super::{next_stop_request, stop_was_requested};

    const FOREVER: Duration = Duration::from_secs(3600);

    #[tokio::test(start_paused = true)]
    async fn a_request_that_arrives_between_stages_is_not_lost() {
        let (requests, mut receiver) = watch::channel(0_u64);
        requests.send_modify(|count| *count += 1);

        tokio::time::timeout(Duration::from_secs(1), next_stop_request(&mut receiver))
            .await
            .expect("a request sent before this stage began waiting must still be seen");
    }

    #[tokio::test(start_paused = true)]
    async fn one_request_does_not_satisfy_two_waits() {
        let (requests, mut receiver) = watch::channel(0_u64);
        requests.send_modify(|count| *count += 1);
        next_stop_request(&mut receiver).await;

        let second = tokio::time::timeout(FOREVER, next_stop_request(&mut receiver)).await;
        assert!(
            second.is_err(),
            "the second stage must wait for a second request"
        );
        // Kept alive to here, so the wait above timed out because no request
        // arrived - not because the sender was gone.
        drop(requests);
    }

    #[tokio::test(start_paused = true)]
    async fn a_listener_that_never_registered_never_requests_a_stop() {
        let (requests, mut receiver) = watch::channel(0_u64);
        drop(requests);

        let waited = tokio::time::timeout(FOREVER, next_stop_request(&mut receiver)).await;
        assert!(
            waited.is_err(),
            "a failed signal registration must not be read as a stop request"
        );
    }

    #[test]
    fn stop_was_requested_counts_requests_whether_or_not_they_were_observed() {
        let (requests, receiver) = watch::channel(0_u64);
        assert!(!stop_was_requested(&receiver));
        requests.send_modify(|count| *count += 1);
        assert!(stop_was_requested(&receiver));
    }
}
