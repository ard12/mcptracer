//! Server-Sent Events `data:` field decoder, shared by `record-http` (T-70)
//! and `replay-http` (T-75). Both need to turn a raw SSE byte stream into
//! JSON-RPC message boundaries, and must disagree about nothing in how they
//! do it or the two commands' recordings could interpret identical bytes
//! differently.

use crate::MAX_FRAME_BYTES;

#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    oversized: bool,
    discarding_line: bool,
}

pub enum SseEvent {
    Json(Vec<u8>),
    Oversized,
    Incomplete,
}

impl SseDecoder {
    /// Whether stopping now would discard an unfinished SSE line/event.
    /// Complete blank-line-delimited events have already been emitted.
    pub fn has_pending_event(&self) -> bool {
        !self.line.is_empty() || !self.data.is_empty() || self.oversized || self.discarding_line
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        // Scan only the new chunk. Re-scanning an accumulated fragmented line
        // or draining a Vec prefix for every short line makes work quadratic.
        for fragment in chunk.split_inclusive(|byte| *byte == b'\n') {
            let complete = fragment.last() == Some(&b'\n');
            let bytes = if complete {
                &fragment[..fragment.len() - 1]
            } else {
                fragment
            };
            if !self.discarding_line {
                // Allow the "data: " prefix and optional trailing CR in
                // addition to the data limit, without buffering an arbitrary
                // incoming chunk before checking its size.
                if self.line.len().saturating_add(bytes.len()) > MAX_FRAME_BYTES + 7 {
                    self.line.clear();
                    self.data.clear();
                    self.oversized = true;
                    self.discarding_line = true;
                } else {
                    self.line.extend_from_slice(bytes);
                }
            }
            if complete {
                if self.discarding_line {
                    // An oversized physical line ends here, not at the chunk
                    // boundary. Its suffix must never become a new data field
                    // or an empty event delimiter.
                    self.discarding_line = false;
                } else {
                    let mut line = std::mem::take(&mut self.line);
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    self.process_line(&line, &mut events);
                    line.clear();
                    self.line = line;
                }
            }
        }
        events
    }

    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        // EOF does not dispatch an SSE event: its final blank line is part
        // of the frame. Never promote a valid-JSON but undelimited prefix.
        if self.oversized {
            events.push(SseEvent::Oversized);
        } else if self.has_pending_event() {
            events.push(SseEvent::Incomplete);
        }
        self.line.clear();
        self.data.clear();
        self.oversized = false;
        self.discarding_line = false;
        events
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.finish_event(events);
            return;
        }
        let Some(mut value) = line.strip_prefix(b"data:") else {
            return;
        };
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        if self.oversized {
            return;
        }
        let separator = usize::from(!self.data.is_empty());
        if self
            .data
            .len()
            .saturating_add(separator)
            .saturating_add(value.len())
            > MAX_FRAME_BYTES
        {
            self.data.clear();
            self.oversized = true;
            return;
        }
        if separator == 1 {
            self.data.push(b'\n');
        }
        self.data.extend_from_slice(value);
    }

    fn finish_event(&mut self, events: &mut Vec<SseEvent>) {
        if self.oversized {
            events.push(SseEvent::Oversized);
        } else if !self.data.is_empty() {
            events.push(SseEvent::Json(std::mem::take(&mut self.data)));
        }
        self.data.clear();
        self.oversized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{SseDecoder, SseEvent};

    #[test]
    fn pending_state_distinguishes_event_boundary_from_unfinished_data() {
        let mut decoder = SseDecoder::default();
        assert!(!decoder.has_pending_event());
        assert!(decoder.push(b"data: dummy").is_empty());
        assert!(decoder.has_pending_event());
        assert!(decoder.push(b"\n").is_empty());
        assert!(decoder.has_pending_event());
        assert_eq!(decoder.push(b"\n").len(), 1);
        assert!(!decoder.has_pending_event());
        assert!(decoder.push(b": keepalive\n\n").is_empty());
        assert!(!decoder.has_pending_event());
        decoder.push(&vec![b'x'; crate::MAX_FRAME_BYTES + 8]);
        assert!(decoder.has_pending_event());
        decoder.finish();
        assert!(!decoder.has_pending_event());
    }

    #[test]
    fn eof_never_promotes_a_json_prefix_without_its_event_delimiter() {
        for suffix in [b"".as_slice(), b"\n"] {
            let mut decoder = SseDecoder::default();
            assert!(decoder
                .push(b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}")
                .is_empty());
            assert!(decoder.push(suffix).is_empty());
            assert!(matches!(
                decoder.finish().as_slice(),
                [SseEvent::Incomplete]
            ));
            assert!(decoder.finish().is_empty());
            assert!(!decoder.has_pending_event());
        }
    }

    #[test]
    fn sse_decoder_reassembles_data_events_across_chunks() {
        let mut decoder = SseDecoder::default();
        let first = br#"id: 4
data: {"jsonrpc":"2.0","#;
        assert!(decoder.push(first).is_empty());
        let second = br#""id":1}

data: {"jsonrpc":"2.0","id":2}

"#;
        let events = decoder.push(second);

        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], SseEvent::Json(value) if value == br#"{"jsonrpc":"2.0","id":1}"#)
        );
        assert!(
            matches!(&events[1], SseEvent::Json(value) if value == br#"{"jsonrpc":"2.0","id":2}"#)
        );
    }

    #[test]
    fn sse_decoder_flags_an_oversized_event() {
        let mut decoder = SseDecoder::default();
        let huge = vec![b'x'; crate::MAX_FRAME_BYTES + 1];
        let mut chunk = b"data: ".to_vec();
        chunk.extend_from_slice(&huge);
        chunk.extend_from_slice(b"\n\n");

        let events = decoder.push(&chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], SseEvent::Oversized));
    }

    #[test]
    fn bytewise_chunks_preserve_crlf_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        let mut events = Vec::new();
        for byte in b": comment\r\ndata: first\r\ndata: second\r\n\r\n" {
            events.extend(decoder.push(std::slice::from_ref(byte)));
        }
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Json(value) if value == b"first\nsecond"));
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn many_short_lines_in_one_chunk_keep_event_order() {
        let mut decoder = SseDecoder::default();
        let chunk = b"data: small\n\n".repeat(1024);
        let events = decoder.push(&chunk);
        assert_eq!(events.len(), 1024);
        assert!(events
            .iter()
            .all(|event| matches!(event, SseEvent::Json(value) if value == b"small")));
    }

    #[test]
    fn oversized_physical_line_discards_continuations_until_newline() {
        let mut decoder = SseDecoder::default();
        assert!(decoder
            .push(&vec![b'x'; crate::MAX_FRAME_BYTES + 8])
            .is_empty());
        assert!(decoder.line.is_empty());
        assert!(decoder.push(b"data: forged").is_empty());
        // This newline ends the oversized line; it is not a blank separator.
        assert!(decoder.push(b"\n").is_empty());
        let events = decoder.push(b"\ndata: next\n\n");
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], SseEvent::Oversized));
        assert!(matches!(&events[1], SseEvent::Json(value) if value == b"next"));
    }

    #[test]
    fn maximum_data_field_is_accepted_even_with_crlf() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: ").is_empty());
        assert!(decoder.push(&vec![b'x'; crate::MAX_FRAME_BYTES]).is_empty());
        let events = decoder.push(b"\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], SseEvent::Json(value) if value.len() == crate::MAX_FRAME_BYTES)
        );
    }

    #[test]
    fn eof_reports_an_oversized_line_once_and_allows_reuse() {
        let mut decoder = SseDecoder::default();
        assert!(decoder
            .push(&vec![b'x'; crate::MAX_FRAME_BYTES + 8])
            .is_empty());
        assert!(matches!(decoder.finish().as_slice(), [SseEvent::Oversized]));
        assert!(decoder.finish().is_empty());
        let events = decoder.push(b"data: recovered\n\n");
        assert!(matches!(events.as_slice(), [SseEvent::Json(value)] if value == b"recovered"));
    }
}
