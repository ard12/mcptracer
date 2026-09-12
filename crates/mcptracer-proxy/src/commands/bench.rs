use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Args;
use mcptracer_model::plan::{
    build_plan, driven_messages as planned_driven_messages, tool_is_allowed,
};
use mcptracer_protocol::{encode_stdio_frame, parse_json_payload, parse_stdio_frame};
use mcptracer_redact::REDACTED_PLACEHOLDER;
use mcptracer_storage::{Store, StoredMessage};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

use crate::session_writer::{spawn_server_process, MAX_FRAME_BYTES};

#[derive(Args)]
pub struct BenchArgs {
    /// Id (or unique prefix) of the recorded session to replay as load.
    pub session_id: String,

    /// Number of session iterations to run.
    #[arg(long, default_value_t = 20)]
    pub repeat: usize,

    /// Maximum number of concurrent session iterations.
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,

    /// Milliseconds to wait for each replayed request response.
    #[arg(long, default_value_t = 30_000)]
    pub request_timeout: u64,

    /// Emit machine-readable JSON.
    #[arg(long)]
    pub json: bool,

    /// Acknowledge that bench re-executes every recorded request for real,
    /// `--repeat` times at up to `--concurrency` concurrent iterations
    /// (deletes, sends, payments, and any other side effect included — this
    /// is not a preview), and that a source session recorded with redaction
    /// sends the literal `***REDACTED***` placeholder as the argument value
    /// rather than the real secret, `--repeat` times over. Suppresses both
    /// warnings printed before every run.
    #[arg(long)]
    pub i_understand_side_effects: bool,

    /// Print a JSON report of the calls this bench run would make (methods,
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

#[derive(Clone)]
struct BenchMessage {
    payload: Value,
    is_request: bool,
    rpc_id: Option<String>,
}

#[derive(Debug, Default)]
struct IterationResult {
    duration_ns: u64,
    request_latencies_ns: Vec<u64>,
    errors: u64,
    unanswered: u64,
}

/// Bump alongside any breaking change to `BenchReport`'s JSON shape and
/// publish a new `schemas/bench-report.vN.schema.json`.
const BENCH_REPORT_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Serialize)]
struct BenchReport {
    schema_version: u32,
    iterations: usize,
    concurrency: usize,
    total_requests: usize,
    total_errors: u64,
    unanswered: u64,
    throughput_sessions_per_sec: f64,
    throughput_requests_per_sec: f64,
    duration_ms: f64,
    latency_p50_ms: Option<f64>,
    latency_p90_ms: Option<f64>,
    latency_p95_ms: Option<f64>,
    latency_p99_ms: Option<f64>,
    latency_max_ms: Option<f64>,
}

pub async fn run(args: BenchArgs, db_path: std::path::PathBuf) -> Result<()> {
    if args.repeat == 0 {
        return Err(anyhow!("--repeat must be greater than 0"));
    }
    if args.concurrency == 0 {
        return Err(anyhow!("--concurrency must be greater than 0"));
    }
    if !args.plan && args.server_args.is_empty() {
        return Err(anyhow!(
            "missing MCP server command after -- (required unless --plan is set)"
        ));
    }

    let store = Store::open(&db_path)?;
    let source = store.get_messages(&args.session_id)?;

    let plan = build_plan(&source, &args.allow_tool, &args.deny_tool);
    if args.plan {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }

    let driven = Arc::new(driven_messages(&source, &args.allow_tool, &args.deny_tool)?);
    if driven.is_empty() {
        return Err(anyhow!(
            "source session has no client-originated requests or notifications to benchmark (after any --allow-tool/--deny-tool filtering)"
        ));
    }
    warn_side_effects(
        &source,
        args.repeat,
        args.concurrency,
        args.i_understand_side_effects,
    );
    warn_redacted_replay(&source, args.repeat, args.i_understand_side_effects);

    let server_args = Arc::new(args.server_args.clone());
    let semaphore = Arc::new(Semaphore::new(args.concurrency));
    let timeout = Duration::from_millis(args.request_timeout);
    let start = Instant::now();
    let mut tasks = Vec::with_capacity(args.repeat);

    for _ in 0..args.repeat {
        let permit = Arc::clone(&semaphore).acquire_owned().await?;
        let driven = Arc::clone(&driven);
        let server_args = Arc::clone(&server_args);
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            run_iteration(&server_args, &driven, timeout).await
        }));
    }

    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        results.push(task.await.context("bench worker panicked")?);
    }

    let report = summarize(args.repeat, args.concurrency, start.elapsed(), &results);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }

    if report.total_errors > 0 || report.unanswered > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Print an unmissable, non-blocking warning naming every distinct call
/// bench is about to re-execute for real against the live target, `--repeat`
/// times at up to `--concurrency` concurrent iterations — bench is a load
/// test against a real server, not a simulation, so a recorded destructive
/// call (delete, send, payment, ...) fires repeatedly and concurrently
/// exactly as recorded. Silences with `--i-understand-side-effects`.
fn warn_side_effects(source: &[StoredMessage], repeat: usize, concurrency: usize, suppress: bool) {
    if suppress {
        return;
    }
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for message in source
        .iter()
        .filter(|m| m.direction == "c2s" && m.message_kind == "request")
    {
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
        "[mcptracer] WARNING: bench re-executes the following recorded call(s) against the live target, {repeat} times at up to {concurrency} concurrent iteration(s):"
    );
    for (label, count) in &counts {
        eprintln!("  {label} (x{count} per iteration)");
    }
    eprintln!(
        "[mcptracer] This has real side effects — not a simulation. Pass --i-understand-side-effects to suppress this warning."
    );
}

/// Print an unmissable, non-blocking warning naming every distinct call
/// whose recorded payload still contains the `***REDACTED***` placeholder —
/// bench sends the stored payload as-is, `--repeat` times, so a redacted
/// argument is sent to the live target verbatim as the literal placeholder
/// string on every iteration, not the original secret. Detects by scanning
/// the actual payload rather than trusting the session's recorded redaction
/// policy, since a `custom` policy may only mask specific keys. Silences
/// with `--i-understand-side-effects`, same flag as `warn_side_effects`.
fn warn_redacted_replay(source: &[StoredMessage], repeat: usize, suppress: bool) {
    if suppress {
        return;
    }
    let counts = redacted_replay_counts(source);
    if counts.is_empty() {
        return;
    }
    eprintln!(
        "[mcptracer] WARNING: {} recorded call(s) still contain the \"{REDACTED_PLACEHOLDER}\" placeholder and will send it verbatim, not the original value, on each of the {repeat} iteration(s):",
        counts.values().sum::<usize>()
    );
    for (label, count) in &counts {
        eprintln!("  {label} (x{count} per iteration)");
    }
    eprintln!(
        "[mcptracer] The live target will see the literal placeholder string, not the redacted secret — this is very unlikely to work as intended. Pass --i-understand-side-effects to suppress this warning."
    );
}

/// Distinct method/tool labels among driven `c2s` requests whose stored
/// payload contains the redaction placeholder, with per-label counts.
fn redacted_replay_counts(source: &[StoredMessage]) -> std::collections::BTreeMap<String, usize> {
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for message in source.iter().filter(|m| {
        m.direction == "c2s" && (m.message_kind == "request" || m.message_kind == "notification")
    }) {
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

fn driven_messages(
    source: &[StoredMessage],
    allow_tools: &[String],
    deny_tools: &[String],
) -> Result<Vec<BenchMessage>> {
    planned_driven_messages(source)
        .into_iter()
        .filter(|m| tool_is_allowed(m.tool_name.as_deref(), allow_tools, deny_tools))
        .map(|msg| {
            let payload: Value = serde_json::from_str(&msg.payload)
                .with_context(|| format!("source message seq {} is not valid JSON", msg.seq))?;
            Ok(BenchMessage {
                payload,
                is_request: msg.message_kind == "request",
                rpc_id: msg.rpc_id.clone(),
            })
        })
        .collect()
}

async fn run_iteration(
    server_args: &[String],
    driven: &[BenchMessage],
    timeout: Duration,
) -> IterationResult {
    let started = Instant::now();
    let mut result = IterationResult::default();

    let mut server = match spawn_server_process(server_args) {
        Ok(server) => server,
        Err(err) => {
            tracing::warn!("bench iteration failed to spawn server: {err}");
            result.errors += 1;
            result.duration_ns = started.elapsed().as_nanos() as u64;
            return result;
        }
    };

    let Some(mut stdin) = server.stdin.take() else {
        result.errors += 1;
        result.duration_ns = started.elapsed().as_nanos() as u64;
        return result;
    };
    let Some(mut stdout) = server.stdout.take() else {
        result.errors += 1;
        result.duration_ns = started.elapsed().as_nanos() as u64;
        return result;
    };

    let mut buf = Vec::with_capacity(64 * 1024);
    for msg in driven {
        let bytes = encode_stdio_frame(&msg.payload);
        if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
            result.errors += 1;
            break;
        }

        if !msg.is_request {
            continue;
        }
        let Some(expected_id) = &msg.rpc_id else {
            continue;
        };

        let request_started = Instant::now();
        match wait_for_response(&mut stdout, &mut buf, expected_id, timeout).await {
            Ok(ResponseOutcome::Ok { is_error }) => {
                result
                    .request_latencies_ns
                    .push(request_started.elapsed().as_nanos() as u64);
                if is_error {
                    result.errors += 1;
                }
            }
            Ok(ResponseOutcome::Timeout) => {
                result.unanswered += 1;
            }
            Err(err) => {
                tracing::warn!("bench iteration failed while waiting for response: {err}");
                result.errors += 1;
                break;
            }
        }
    }

    let _ = server.kill().await;
    let _ = server.wait().await;
    result.duration_ns = started.elapsed().as_nanos() as u64;
    result
}

enum ResponseOutcome {
    Ok { is_error: bool },
    Timeout,
}

async fn wait_for_response(
    stdout: &mut tokio::process::ChildStdout,
    buf: &mut Vec<u8>,
    expected_rpc_id: &str,
    timeout: Duration,
) -> Result<ResponseOutcome> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        while let Some(frame) = parse_stdio_frame(buf)? {
            let consumed = frame.consumed;
            let payload = parse_json_payload(&frame.json_bytes)?;
            let id_matches =
                payload.get("id").map(Value::to_string).as_deref() == Some(expected_rpc_id);
            let is_response = payload.get("id").is_some() && payload.get("method").is_none();
            let is_error = payload.get("error").is_some()
                || payload.pointer("/result/isError").and_then(Value::as_bool) == Some(true);
            buf.drain(..consumed);
            if is_response && id_matches {
                return Ok(ResponseOutcome::Ok { is_error });
            }
        }

        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(ResponseOutcome::Timeout);
        }

        let mut tmp = [0_u8; 4096];
        match tokio::time::timeout(remaining, stdout.read(&mut tmp)).await {
            Ok(Ok(0)) => return Ok(ResponseOutcome::Timeout),
            Ok(Ok(n)) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.len() > MAX_FRAME_BYTES {
                    return Err(anyhow!(
                        "server frame exceeded {} bytes without a newline",
                        MAX_FRAME_BYTES
                    ));
                }
            }
            Ok(Err(err)) => return Err(anyhow!("failed to read server stdout: {err}")),
            Err(_) => return Ok(ResponseOutcome::Timeout),
        }
    }
}

fn summarize(
    iterations: usize,
    concurrency: usize,
    elapsed: Duration,
    results: &[IterationResult],
) -> BenchReport {
    let mut latencies: Vec<u64> = results
        .iter()
        .flat_map(|r| r.request_latencies_ns.iter().copied())
        .collect();
    latencies.sort_unstable();

    let total_requests = latencies.len();
    let total_errors = results.iter().map(|r| r.errors).sum();
    let unanswered = results.iter().map(|r| r.unanswered).sum();
    let duration_secs = elapsed.as_secs_f64().max(f64::EPSILON);

    BenchReport {
        schema_version: BENCH_REPORT_SCHEMA_VERSION,
        iterations,
        concurrency,
        total_requests,
        total_errors,
        unanswered,
        throughput_sessions_per_sec: iterations as f64 / duration_secs,
        throughput_requests_per_sec: total_requests as f64 / duration_secs,
        duration_ms: elapsed.as_secs_f64() * 1000.0,
        latency_p50_ms: percentile_ms(&latencies, 0.50),
        latency_p90_ms: percentile_ms(&latencies, 0.90),
        latency_p95_ms: percentile_ms(&latencies, 0.95),
        latency_p99_ms: percentile_ms(&latencies, 0.99),
        latency_max_ms: latencies.last().map(|ns| *ns as f64 / 1_000_000.0),
    }
}

fn percentile_ms(values: &[u64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let idx = ((values.len() - 1) as f64 * p).ceil() as usize;
    values.get(idx).map(|ns| *ns as f64 / 1_000_000.0)
}

fn print_report(report: &BenchReport) {
    println!(
        "iterations                 {} (concurrency {})",
        report.iterations, report.concurrency
    );
    println!("duration                   {:.1} ms", report.duration_ms);
    println!(
        "throughput                 {:.2} sessions/s, {:.2} requests/s",
        report.throughput_sessions_per_sec, report.throughput_requests_per_sec
    );
    println!(
        "requests/errors/unanswered {} / {} / {}",
        report.total_requests, report.total_errors, report.unanswered
    );
    println!(
        "latency                    p50={} p90={} p95={} p99={} max={}",
        fmt_ms(report.latency_p50_ms),
        fmt_ms(report.latency_p90_ms),
        fmt_ms(report.latency_p95_ms),
        fmt_ms(report.latency_p99_ms),
        fmt_ms(report.latency_max_ms)
    );
}

fn fmt_ms(value: Option<f64>) -> String {
    value
        .map(|ms| format!("{ms:.2}ms"))
        .unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_request(
        seq: u64,
        method: &str,
        tool_name: Option<&str>,
        payload: &str,
    ) -> StoredMessage {
        StoredMessage {
            seq,
            ts_ns: 0,
            direction: "c2s".to_string(),
            message_kind: "request".to_string(),
            rpc_id: Some(seq.to_string()),
            method: Some(method.to_string()),
            tool_name: tool_name.map(String::from),
            payload: payload.to_string(),
            payload_bytes: payload.len(),
            is_error: false,
            error_code: None,
        }
    }

    #[test]
    fn redacted_replay_counts_flags_only_calls_containing_the_placeholder() {
        let clean_call = stored_request(
            0,
            "tools/call",
            Some("echo"),
            r#"{"jsonrpc":"2.0","id":0,"params":{"name":"echo","arguments":{"text":"hi"}}}"#,
        );
        let redacted_payload = format!(
            r#"{{"jsonrpc":"2.0","id":1,"params":{{"name":"send_email","arguments":{{"api_key":"{REDACTED_PLACEHOLDER}"}}}}}}"#
        );
        let redacted_call = stored_request(1, "tools/call", Some("send_email"), &redacted_payload);
        let redacted_call_again =
            stored_request(2, "tools/call", Some("send_email"), &redacted_payload);

        let source = vec![clean_call, redacted_call, redacted_call_again];
        let counts = redacted_replay_counts(&source);

        assert_eq!(counts.len(), 1);
        assert_eq!(counts.get("tools/call send_email"), Some(&2));
        assert!(!counts.contains_key("tools/call echo"));
    }

    #[test]
    fn redacted_replay_counts_is_empty_when_nothing_is_redacted() {
        let call = stored_request(0, "tools/list", None, r#"{"jsonrpc":"2.0","id":0}"#);
        assert!(redacted_replay_counts(&[call]).is_empty());
    }

    #[test]
    fn summarize_reports_percentiles_and_throughput() {
        let results = vec![
            IterationResult {
                duration_ns: 10,
                request_latencies_ns: vec![1_000_000, 2_000_000],
                errors: 0,
                unanswered: 0,
            },
            IterationResult {
                duration_ns: 20,
                request_latencies_ns: vec![3_000_000, 4_000_000],
                errors: 1,
                unanswered: 1,
            },
        ];

        let report = summarize(2, 2, Duration::from_secs(1), &results);

        assert_eq!(report.total_requests, 4);
        assert_eq!(report.total_errors, 1);
        assert_eq!(report.unanswered, 1);
        assert_eq!(report.latency_p50_ms, Some(3.0));
        assert_eq!(report.latency_p90_ms, Some(4.0));
        assert_eq!(report.latency_p95_ms, Some(4.0));
        assert_eq!(report.latency_p99_ms, Some(4.0));
        assert_eq!(report.latency_max_ms, Some(4.0));
    }

    #[test]
    fn summarize_stamps_the_current_schema_version() {
        let report = summarize(1, 1, Duration::from_secs(1), &[IterationResult::default()]);
        assert_eq!(report.schema_version, BENCH_REPORT_SCHEMA_VERSION);
    }
}
