//! Conservative request context for schema-aware redaction.
use std::collections::HashSet;

use serde_json::Value;

/// Payload shape alone never grants a schema exception.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RedactionContext {
    #[default]
    Runtime,
    /// A response matched to a previously observed tools/list request.
    ToolListResponse,
}

/// Bounded, direction-scoped correlation shared by recording and validation.
/// Gaps, malformed messages and missing requests fail closed to runtime rules.
#[derive(Default)]
pub struct RedactionContextTracker {
    pending: HashSet<(bool, String)>,
    previous_seq: Option<u64>,
    lost_context: bool,
}

const MAX_PENDING_LIST_REQUESTS: usize = 4096;
const MAX_TRACKED_ID_BYTES: usize = 1024;

impl RedactionContextTracker {
    /// True when bounded retention could not track a valid tools/list request.
    /// Callers must expose conservative schema fallback as incomplete evidence.
    pub fn has_lost_context(&self) -> bool {
        self.lost_context
    }

    pub fn observe(&mut self, seq: u64, direction: &str, payload: &Value) -> RedactionContext {
        if self
            .previous_seq
            .is_some_and(|previous| previous.checked_add(1) != Some(seq))
        {
            self.pending.clear();
        }
        self.previous_seq = Some(seq);
        let responder = match direction {
            "c2s" => false,
            "s2c" => true,
            _ => {
                self.pending.clear();
                return RedactionContext::Runtime;
            }
        };
        let Some(id) = payload
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
        else {
            return RedactionContext::Runtime;
        };
        let list_request = payload.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
            && payload.get("method").and_then(Value::as_str) == Some("tools/list")
            && payload.get("result").is_none()
            && payload.get("error").is_none();
        if id
            .as_str()
            .is_some_and(|text| text.len() > MAX_TRACKED_ID_BYTES)
        {
            self.lost_context |= list_request;
            return RedactionContext::Runtime;
        }
        let encoded = id.to_string();
        if encoded.len() > MAX_TRACKED_ID_BYTES {
            self.lost_context |= list_request;
            return RedactionContext::Runtime;
        }
        if payload.get("method").is_some() {
            let key = (!responder, encoded);
            self.pending.remove(&key);
            if list_request {
                if self.pending.len() < MAX_PENDING_LIST_REQUESTS {
                    self.pending.insert(key);
                } else {
                    self.lost_context = true;
                }
            }
            RedactionContext::Runtime
        } else {
            let matched = self.pending.remove(&(responder, encoded));
            if matched
                && payload.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
                && payload.get("result").is_some()
                && payload.get("error").is_none()
            {
                RedactionContext::ToolListResponse
            } else {
                RedactionContext::Runtime
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_direction_and_invalidates_reused_request_ids() {
        let mut tracker = RedactionContextTracker::default();
        tracker.observe(
            0,
            "c2s",
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        );
        tracker.observe(
            1,
            "s2c",
            &json!({"jsonrpc":"2.0","id":1,"method":"roots/list"}),
        );
        assert_eq!(
            tracker.observe(2, "c2s", &json!({"jsonrpc":"2.0","id":1,"result":{}})),
            RedactionContext::Runtime
        );
        assert_eq!(
            tracker.observe(3, "s2c", &json!({"jsonrpc":"2.0","id":1,"result":{}})),
            RedactionContext::ToolListResponse
        );
        tracker.observe(
            4,
            "c2s",
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        );
        tracker.observe(
            5,
            "c2s",
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call"}),
        );
        assert_eq!(
            tracker.observe(6, "s2c", &json!({"jsonrpc":"2.0","id":1,"result":{}})),
            RedactionContext::Runtime
        );
    }

    #[test]
    fn gaps_errors_orphans_and_string_ids_fail_closed() {
        let mut tracker = RedactionContextTracker::default();
        tracker.observe(
            0,
            "c2s",
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        );
        assert_eq!(
            tracker.observe(2, "s2c", &json!({"jsonrpc":"2.0","id":1,"result":{}})),
            RedactionContext::Runtime
        );
        tracker.observe(
            3,
            "c2s",
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        );
        assert_eq!(
            tracker.observe(4, "s2c", &json!({"jsonrpc":"2.0","id":"1","result":{}})),
            RedactionContext::Runtime
        );
        assert_eq!(
            tracker.observe(5, "s2c", &json!({"jsonrpc":"2.0","id":1,"error":{}})),
            RedactionContext::Runtime
        );
        assert_eq!(
            tracker.observe(6, "s2c", &json!({"jsonrpc":"2.0","id":1,"result":{}})),
            RedactionContext::Runtime
        );
    }

    #[test]
    fn context_retention_is_bounded() {
        let mut tracker = RedactionContextTracker::default();
        for id in 0..MAX_PENDING_LIST_REQUESTS + 3 {
            tracker.observe(
                id as u64,
                "c2s",
                &json!({"jsonrpc":"2.0","id":id,"method":"tools/list"}),
            );
        }
        assert_eq!(tracker.pending.len(), MAX_PENDING_LIST_REQUESTS);
        assert!(tracker.has_lost_context());
        let seq = MAX_PENDING_LIST_REQUESTS as u64 + 3;
        assert_eq!(
            tracker.observe(
                seq,
                "s2c",
                &json!({"jsonrpc":"2.0","id":MAX_PENDING_LIST_REQUESTS,"result":{}})
            ),
            RedactionContext::Runtime
        );
        tracker.observe(
            seq + 1,
            "c2s",
            &json!({"jsonrpc":"2.0","id":"x".repeat(MAX_TRACKED_ID_BYTES+1),"method":"tools/list"}),
        );
        assert_eq!(tracker.pending.len(), MAX_PENDING_LIST_REQUESTS);
    }

    #[test]
    fn ambiguous_requests_cannot_grant_schema_context() {
        for extra in ["result", "error"] {
            let mut tracker = RedactionContextTracker::default();
            let mut request = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
            request[extra] = json!({});
            tracker.observe(0, "c2s", &request);
            assert_eq!(
                tracker.observe(1, "s2c", &json!({"jsonrpc":"2.0","id":1,"result":{}})),
                RedactionContext::Runtime
            );
        }
    }
}
