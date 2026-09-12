//! Server-Sent Events `data:` field decoder, shared by `record-http` (T-70)
//! and `replay-http` (T-75) — both need to turn a raw SSE byte stream into
//! JSON-RPC message boundaries, and must disagree about nothing in how they
//! do it or the two commands' recordings could interpret identical bytes
//! differently.

use crate::session_writer::MAX_FRAME_BYTES;

#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    oversized: bool,
}

pub enum SseEvent {
    Json(Vec<u8>),
    Oversized,
}

impl SseDecoder {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.line.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.line.iter().position(|byte| *byte == b'\n') {
            let mut line = self.line.drain(..=newline).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.process_line(&line, &mut events);
        }
        if self.line.len() > MAX_FRAME_BYTES {
            self.line.clear();
            self.oversized = true;
        }
        events
    }

    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.process_line(&line, &mut events);
        }
        self.finish_event(&mut events);
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
        let huge = vec![b'x'; crate::session_writer::MAX_FRAME_BYTES + 1];
        let mut chunk = b"data: ".to_vec();
        chunk.extend_from_slice(&huge);
        chunk.extend_from_slice(b"\n\n");

        let events = decoder.push(&chunk);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], SseEvent::Oversized));
    }
}
