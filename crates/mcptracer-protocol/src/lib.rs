use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    /// Matches [`Direction::as_db_str`], the `.mtrace` format, and the
    /// SQLite `direction` column's `"c2s"` encoding.
    #[serde(rename = "c2s")]
    ClientToServer,
    /// Matches [`Direction::as_db_str`], the `.mtrace` format, and the
    /// SQLite `direction` column's `"s2c"` encoding.
    #[serde(rename = "s2c")]
    ServerToClient,
}

impl Direction {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Direction::ClientToServer => "c2s",
            Direction::ServerToClient => "s2c",
        }
    }

    /// Parse the database/wire encoding produced by [`Direction::as_db_str`].
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "c2s" => Some(Direction::ClientToServer),
            "s2c" => Some(Direction::ServerToClient),
            _ => None,
        }
    }

    /// The direction a reply travels: a client→server request is answered by a
    /// server→client response, and vice versa.
    pub fn opposite(self) -> Self {
        match self {
            Direction::ClientToServer => Direction::ServerToClient,
            Direction::ServerToClient => Direction::ClientToServer,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpMessage {
    pub seq: u64,
    pub timestamp_ns: i64,
    pub direction: Direction,
    pub payload: Value,
    pub payload_bytes: usize,
}

/// A transport-specific boundary between wire framing and MCPTracer's shared
/// JSON-RPC message model. Implementations receive one complete JSON payload;
/// they do not own I/O or storage.
pub trait Transport {
    fn name(&self) -> &'static str;

    fn decode_message(
        &self,
        bytes: &[u8],
        direction: Direction,
        seq: u64,
        timestamp_ns: i64,
    ) -> Result<McpMessage, TransportDecodeError>;
}

#[derive(Debug, Error)]
pub enum TransportDecodeError {
    #[error("invalid JSON-RPC payload: {0}")]
    Json(#[from] serde_json::Error),
}

/// Newline-delimited stdio framing yields complete JSON payloads for this
/// decoder. Framing itself remains in [`parse_stdio_frame`].
#[derive(Debug, Default)]
pub struct StdioTransport;

/// Streamable HTTP `POST` bodies and SSE `data:` events yield complete JSON
/// payloads for this decoder. HTTP and SSE framing remain transport-owned.
#[derive(Debug, Default)]
pub struct StreamableHttpTransport;

impl Transport for StdioTransport {
    fn name(&self) -> &'static str {
        "stdio"
    }

    fn decode_message(
        &self,
        bytes: &[u8],
        direction: Direction,
        seq: u64,
        timestamp_ns: i64,
    ) -> Result<McpMessage, TransportDecodeError> {
        decode_json_message(bytes, direction, seq, timestamp_ns)
    }
}

impl Transport for StreamableHttpTransport {
    fn name(&self) -> &'static str {
        "streamable-http"
    }

    fn decode_message(
        &self,
        bytes: &[u8],
        direction: Direction,
        seq: u64,
        timestamp_ns: i64,
    ) -> Result<McpMessage, TransportDecodeError> {
        decode_json_message(bytes, direction, seq, timestamp_ns)
    }
}

fn decode_json_message(
    bytes: &[u8],
    direction: Direction,
    seq: u64,
    timestamp_ns: i64,
) -> Result<McpMessage, TransportDecodeError> {
    Ok(McpMessage {
        seq,
        timestamp_ns,
        direction,
        payload: serde_json::from_slice(bytes)?,
        payload_bytes: bytes.len(),
    })
}

impl McpMessage {
    pub fn method(&self) -> Option<&str> {
        self.payload.get("method")?.as_str()
    }

    pub fn id(&self) -> Option<&Value> {
        self.payload.get("id")
    }

    pub fn id_json(&self) -> Option<String> {
        self.id().map(Value::to_string)
    }

    pub fn tool_name(&self) -> Option<&str> {
        if self.method()? != "tools/call" {
            return None;
        }

        self.payload.get("params")?.get("name")?.as_str()
    }

    pub fn is_error(&self) -> bool {
        self.payload.get("error").is_some()
    }

    /// Whether this is a successful JSON-RPC response whose tool execution
    /// reported a failure. This is intentionally distinct from [`Self::is_error`]:
    /// MCP represents tool failures inside `result.isError`, not in the
    /// JSON-RPC `error` member.
    pub fn is_tool_error(&self) -> bool {
        self.payload
            .pointer("/result/isError")
            .and_then(Value::as_bool)
            == Some(true)
    }

    pub fn error_code(&self) -> Option<i64> {
        self.payload.get("error")?.get("code")?.as_i64()
    }

    pub fn is_notification(&self) -> bool {
        self.id().is_none() && self.method().is_some()
    }

    pub fn is_response(&self) -> bool {
        self.id().is_some() && self.method().is_none()
    }

    pub fn kind(&self) -> MessageKind {
        if self.is_response() {
            MessageKind::Response
        } else if self.is_notification() {
            MessageKind::Notification
        } else {
            MessageKind::Request
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Response,
    Notification,
}

impl MessageKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            MessageKind::Request => "request",
            MessageKind::Response => "response",
            MessageKind::Notification => "notification",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioFrame {
    pub json_bytes: Vec<u8>,
    pub wire_bytes: Vec<u8>,
    pub consumed: usize,
}

pub fn parse_stdio_frame(buf: &[u8]) -> anyhow::Result<Option<StdioFrame>> {
    let Some(newline_idx) = buf.iter().position(|b| *b == b'\n') else {
        return Ok(None);
    };

    let consumed = newline_idx + 1;
    let wire_bytes = buf[..consumed].to_vec();
    let mut json_bytes = buf[..newline_idx].to_vec();

    if json_bytes.last() == Some(&b'\r') {
        json_bytes.pop();
    }

    if json_bytes.is_empty() {
        anyhow::bail!("empty stdio frame");
    }

    Ok(Some(StdioFrame {
        json_bytes,
        wire_bytes,
        consumed,
    }))
}

pub fn encode_stdio_frame(payload: &Value) -> Vec<u8> {
    let mut out = payload.to_string().into_bytes();
    out.push(b'\n');
    out
}

pub fn parse_json_payload(bytes: &[u8]) -> anyhow::Result<Value> {
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn json_values() -> BoxedStrategy<Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|number| Value::Number(number.into())),
            proptest::collection::vec(any::<char>(), 0..32)
                .prop_map(|chars| Value::String(chars.into_iter().collect())),
        ];

        leaf.prop_recursive(4, 64, 8, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..8).prop_map(Value::Array),
                proptest::collection::btree_map(
                    proptest::collection::vec(any::<char>(), 0..16)
                        .prop_map(|chars| chars.into_iter().collect::<String>()),
                    inner,
                    0..8,
                )
                .prop_map(|map: BTreeMap<String, Value>| {
                    Value::Object(map.into_iter().collect())
                }),
            ]
        })
        .boxed()
    }

    proptest! {
        #[test]
        fn parse_stdio_frame_never_panics_for_arbitrary_bytes(
            input in proptest::collection::vec(any::<u8>(), 0..8192),
        ) {
            let _ = parse_stdio_frame(&input);
        }

        #[test]
        fn encoded_json_payloads_round_trip(payload in json_values()) {
            let encoded = encode_stdio_frame(&payload);
            let decoded = parse_json_payload(&encoded[..encoded.len() - 1]).unwrap();

            prop_assert_eq!(decoded, payload);
        }
    }

    #[test]
    fn parses_newline_delimited_stdio_frame() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let mut frame = body.to_vec();
        frame.push(b'\n');

        let parsed = parse_stdio_frame(&frame).unwrap().unwrap();

        assert_eq!(parsed.json_bytes, body);
        assert_eq!(parsed.wire_bytes, frame);
        assert_eq!(parsed.consumed, frame.len());
    }

    #[test]
    fn parses_crlf_stdio_frame_for_json_but_preserves_wire() {
        let frame = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let mut wire = frame.to_vec();
        wire.extend_from_slice(b"\r\n");

        let parsed = parse_stdio_frame(&wire).unwrap().unwrap();

        assert_eq!(parsed.json_bytes, frame);
        assert_eq!(parsed.wire_bytes, wire);
    }

    #[test]
    fn incomplete_frame_returns_none() {
        let result = parse_stdio_frame(br#"{"jsonrpc":"2.0""#).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn empty_frame_is_rejected() {
        assert!(parse_stdio_frame(b"\n").is_err());
        assert!(parse_stdio_frame(b"\r\n").is_err());
    }

    #[test]
    fn extracts_tool_name_only_for_tool_calls() {
        let msg = McpMessage {
            seq: 1,
            timestamp_ns: 0,
            direction: Direction::ClientToServer,
            payload: json!({
                "jsonrpc": "2.0",
                "id": "abc",
                "method": "tools/call",
                "params": {"name": "read_file", "arguments": {"path": "/tmp/a"}}
            }),
            payload_bytes: 0,
        };

        assert_eq!(msg.method(), Some("tools/call"));
        assert_eq!(msg.id_json().as_deref(), Some("\"abc\""));
        assert_eq!(msg.tool_name(), Some("read_file"));
        assert_eq!(msg.kind(), MessageKind::Request);
    }

    #[test]
    fn direction_db_str_round_trips_and_opposite() {
        for dir in [Direction::ClientToServer, Direction::ServerToClient] {
            assert_eq!(Direction::from_db_str(dir.as_db_str()), Some(dir));
            assert_eq!(dir.opposite().opposite(), dir);
            assert_ne!(dir.opposite(), dir);
        }
        assert_eq!(Direction::from_db_str("bogus"), None);
    }

    /// `Direction`'s JSON encoding (used only for `Exchange.origin` and
    /// `NotificationEvent.direction` in the public JSON API) must match the
    /// `.mtrace`/SQLite wire encoding this project uses everywhere else —
    /// `"c2s"`/`"s2c"`, not `"client_to_server"` or the derive default
    /// `"ClientToServer"`.
    #[test]
    fn direction_serializes_as_the_wire_encoding_and_round_trips() {
        assert_eq!(
            serde_json::to_value(Direction::ClientToServer).unwrap(),
            json!("c2s")
        );
        assert_eq!(
            serde_json::to_value(Direction::ServerToClient).unwrap(),
            json!("s2c")
        );
        for dir in [Direction::ClientToServer, Direction::ServerToClient] {
            let value = serde_json::to_value(dir).unwrap();
            let round_tripped: Direction = serde_json::from_value(value).unwrap();
            assert_eq!(round_tripped, dir);
        }
    }

    #[test]
    fn transports_decode_complete_json_payloads_into_the_shared_model() {
        let payload = br#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#;

        for transport in [&StdioTransport, &StreamableHttpTransport] as [&dyn Transport; 2] {
            let message = transport
                .decode_message(payload, Direction::ClientToServer, 3, 17)
                .unwrap();
            assert_eq!(message.seq, 3);
            assert_eq!(message.timestamp_ns, 17);
            assert_eq!(message.payload_bytes, payload.len());
            assert_eq!(message.method(), Some("tools/list"));
        }
    }

    #[test]
    fn encodes_stdio_frame_with_trailing_newline() {
        let payload = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
        let encoded = encode_stdio_frame(&payload);
        assert_eq!(encoded.last(), Some(&b'\n'));

        let parsed = parse_stdio_frame(&encoded).unwrap().unwrap();
        let decoded = parse_json_payload(&parsed.json_bytes).unwrap();
        assert_eq!(decoded, payload);
    }
}
