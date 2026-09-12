use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use mcptracer_model::serve::MockSession;
use mcptracer_protocol::{encode_stdio_frame, parse_json_payload, parse_stdio_frame};
use mcptracer_storage::Store;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// CLI-facing mirror of [`mcptracer_model::serve::MatchStrategy`]; `clap`
/// stays out of `mcptracer-model`'s dependency tree.
#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum MatchStrategy {
    Exact,
    Method,
    #[default]
    MethodAndParams,
    Subset,
    Sequential,
}

impl From<MatchStrategy> for mcptracer_model::serve::MatchStrategy {
    fn from(value: MatchStrategy) -> Self {
        match value {
            MatchStrategy::Exact => Self::Exact,
            MatchStrategy::Method => Self::Method,
            MatchStrategy::MethodAndParams => Self::MethodAndParams,
            MatchStrategy::Subset => Self::Subset,
            MatchStrategy::Sequential => Self::Sequential,
        }
    }
}

#[derive(Args)]
pub struct ServeArgs {
    /// Id (or unique prefix) of a recorded stdio session to serve.
    pub session_id: String,

    /// Reply with an error for an unmatched request, then exit non-zero.
    #[arg(long)]
    pub strict: bool,

    /// How a live request is paired with a recorded exchange.
    #[arg(long, value_enum, default_value_t = MatchStrategy::MethodAndParams)]
    pub match_strategy: MatchStrategy,
}

/// Run an offline stdio MCP server from the answered, client-originated
/// exchanges in a recorded session. It never starts or contacts the source
/// server, and stdout remains reserved exclusively for protocol frames.
pub async fn run(args: ServeArgs, db_path: PathBuf) -> Result<()> {
    let store = Store::open(&db_path)?;
    let messages = store.get_messages(&args.session_id)?;
    let mut mock = MockSession::from_messages_with_strategy(&messages, args.match_strategy.into())
        .context("could not build a serve-mode response inventory from the session")?;
    if mock.is_empty() {
        return Err(anyhow!(
            "session {} has no answered client-originated requests to serve",
            args.session_id
        ));
    }

    eprintln!(
        "[mcptracer] serving recorded session {} from {}",
        args.session_id,
        db_path.display()
    );

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut tmp = [0_u8; 4096];

    loop {
        let bytes_read = stdin
            .read(&mut tmp)
            .await
            .context("failed to read MCP client stdin")?;
        if bytes_read == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..bytes_read]);

        while let Some(frame) = parse_stdio_frame(&buf)? {
            let consumed = frame.consumed;
            let request = match parse_json_payload(&frame.json_bytes) {
                Ok(request) => request,
                Err(error) => {
                    buf.drain(..consumed);
                    eprintln!("[mcptracer] ignored malformed client JSON-RPC frame: {error}");
                    if args.strict {
                        return Err(anyhow!("received malformed client JSON-RPC frame"));
                    }
                    continue;
                }
            };
            buf.drain(..consumed);
            handle_request(&mut stdout, &mut mock, request, args.strict).await?;
        }

        if buf.len() > crate::session_writer::MAX_FRAME_BYTES {
            return Err(anyhow!(
                "client frame exceeded {} bytes without a newline",
                crate::session_writer::MAX_FRAME_BYTES
            ));
        }
    }
}

async fn handle_request<W>(
    stdout: &mut W,
    mock: &mut MockSession,
    request: Value,
    strict: bool,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        eprintln!("[mcptracer] ignored client frame without a JSON-RPC method");
        if strict {
            return Err(anyhow!("received client frame without a JSON-RPC method"));
        }
        return Ok(());
    };
    let Some(id) = request.get("id").cloned() else {
        // Notifications intentionally never receive a JSON-RPC response.
        return Ok(());
    };

    if let Some(selected) = mock.match_request(&request) {
        let response = response_with_id(selected.response, id)?;
        send_frame(stdout, &response).await?;
        return Ok(());
    }

    let response = unmatched_response(id, method);
    send_frame(stdout, &response).await?;
    eprintln!(
        "[mcptracer] no recorded response matched client request method={} id={}",
        method,
        request.get("id").map(Value::to_string).unwrap_or_default()
    );
    if strict {
        return Err(anyhow!(
            "no recorded response matched client request method={method}; --strict is set"
        ));
    }
    Ok(())
}

fn response_with_id(mut response: Value, id: Value) -> Result<Value> {
    let object = response
        .as_object_mut()
        .ok_or_else(|| anyhow!("stored response is not a JSON object"))?;
    object.insert("id".to_string(), id);
    Ok(response)
}

fn unmatched_response(id: Value, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32601,
            "message": format!("No recorded response for method: {method}"),
        }
    })
}

async fn send_frame<W>(stdout: &mut W, value: &Value) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    stdout
        .write_all(&encode_stdio_frame(value))
        .await
        .context("failed to write mock response to client stdout")?;
    stdout
        .flush()
        .await
        .context("failed to flush mock response to client stdout")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{response_with_id, unmatched_response};

    #[test]
    fn selected_response_keeps_body_and_uses_live_request_id() {
        let response = response_with_id(
            json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}}),
            json!("fresh"),
        )
        .unwrap();

        assert_eq!(response["id"], "fresh");
        assert_eq!(response["result"]["ok"], true);
    }

    #[test]
    fn unmatched_response_is_a_json_rpc_method_error() {
        let response = unmatched_response(json!(42), "tools/call");

        assert_eq!(response["id"], 42);
        assert_eq!(response["error"]["code"], -32601);
    }
}
