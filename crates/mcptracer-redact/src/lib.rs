//! Payload redaction for MCPTracer.
//!
//! Redaction rewrites the copy of a message that gets **stored**. It never
//! touches the bytes forwarded between the MCP client and server: the real
//! server needs the real payload, so forwarding stays byte-exact and redaction
//! applies only on the recording path.
//!
//! The default policy is key-name based and deliberately conservative: values
//! under keys that commonly carry secrets (passwords, tokens, API keys,
//! credentials, cookies) are replaced with a fixed placeholder. Structural keys
//! used by MCPTracer for indexing (`method`, `id`, `name`) are never sensitive,
//! so redaction does not interfere with method/tool/id extraction.

use serde_json::Value;

/// Placeholder substituted for a redacted value.
pub const REDACTED_PLACEHOLDER: &str = "***REDACTED***";

/// Sensitive when the normalized key *contains* one of these fragments.
/// Each fragment is specific enough that substring matching does not cause
/// obvious false positives (e.g. "author" does not contain any of them).
const CONTAINS_TOKENS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "apikey",
    "accesstoken",
    "refreshtoken",
    "authorization",
    "credential",
    "privatekey",
    "clientsecret",
    "sessiontoken",
    "mnemonic",
    "seedphrase",
];

/// Sensitive when the normalized key *equals* or *ends with* one of these.
/// Used for short fragments where substring matching would over-match.
const SUFFIX_TOKENS: &[&str] = &["token", "cookie", "bearer"];

/// Which redaction policy to apply on the recording path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedactionPolicy {
    /// Store payloads verbatim.
    None,
    /// Mask values under common secret-bearing keys.
    Default,
    /// Mask the default keys plus explicitly named custom keys.
    Custom { extra_keys: Vec<String> },
}

impl RedactionPolicy {
    /// Parse a policy from its CLI/database name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "none" => Some(RedactionPolicy::None),
            "default" => Some(RedactionPolicy::Default),
            _ => None,
        }
    }

    /// Build a policy from the command-line options used by `record` and
    /// `replay`. Custom keys deliberately augment the standard default policy;
    /// they cannot be combined with `--redact none` by accident.
    pub fn from_cli(name: &str, extra_keys: &[String]) -> Result<Self, String> {
        let base = Self::from_name(name)
            .ok_or_else(|| format!("unknown redaction policy: {name} (expected none|default)"))?;
        if extra_keys.is_empty() {
            return Ok(base);
        }
        if !matches!(base, RedactionPolicy::Default) {
            return Err("--redact-keys requires --redact default".to_string());
        }

        let mut normalized_keys: Vec<String> = extra_keys
            .iter()
            .map(|key| normalize_key(key))
            .filter(|key| !key.is_empty())
            .collect();
        normalized_keys.sort();
        normalized_keys.dedup();
        if normalized_keys.is_empty() {
            return Err("--redact-keys must contain at least one non-empty key".to_string());
        }

        Ok(RedactionPolicy::Custom {
            extra_keys: normalized_keys,
        })
    }

    /// Stable string used for CLI values and the `sessions.redaction_policy`
    /// column.
    pub fn as_str(&self) -> &'static str {
        match self {
            RedactionPolicy::None => "none",
            RedactionPolicy::Default => "default",
            RedactionPolicy::Custom { .. } => "custom",
        }
    }

    /// Reconstruct a policy from durable session metadata. Custom key names
    /// are normalized exactly as CLI input is normalized, so validation and
    /// recording use the same matching rule.
    pub fn from_storage(name: &str, extra_keys: &[String]) -> Result<Self, String> {
        match name {
            "none" => {
                if extra_keys.is_empty() {
                    Ok(RedactionPolicy::None)
                } else {
                    Err("the none redaction policy cannot have custom keys".to_string())
                }
            }
            "default" => {
                if extra_keys.is_empty() {
                    Ok(RedactionPolicy::Default)
                } else {
                    Err("the default redaction policy cannot have custom keys".to_string())
                }
            }
            "custom" => {
                if extra_keys.is_empty() {
                    return Err("the custom redaction policy requires custom keys".to_string());
                }
                Self::from_cli("default", extra_keys)
            }
            _ => Err(format!(
                "unknown redaction policy in stored session: {name}"
            )),
        }
    }

    /// Normalized custom keys that are part of this policy. Empty for `none`
    /// and `default` policies.
    pub fn custom_keys(&self) -> &[String] {
        match self {
            RedactionPolicy::Custom { extra_keys } => extra_keys,
            RedactionPolicy::None | RedactionPolicy::Default => &[],
        }
    }
}

/// Applies a [`RedactionPolicy`] to `serde_json` payloads in place.
#[derive(Debug, Clone)]
pub struct Redactor {
    policy: RedactionPolicy,
    custom_keys: Vec<String>,
    placeholder: Value,
}

impl Redactor {
    /// Build a redactor for the given policy.
    pub fn new(policy: RedactionPolicy) -> Self {
        let custom_keys = match &policy {
            RedactionPolicy::Custom { extra_keys } => extra_keys
                .iter()
                .map(|key| normalize_key(key))
                .filter(|key| !key.is_empty())
                .collect(),
            RedactionPolicy::None | RedactionPolicy::Default => Vec::new(),
        };
        Self {
            policy,
            custom_keys,
            placeholder: Value::String(REDACTED_PLACEHOLDER.to_string()),
        }
    }

    /// The policy this redactor enforces.
    pub fn policy(&self) -> &RedactionPolicy {
        &self.policy
    }

    /// True if this redactor rewrites anything. A [`RedactionPolicy::None`]
    /// redactor is a no-op and callers can skip cloning payloads for it.
    pub fn is_active(&self) -> bool {
        !matches!(&self.policy, RedactionPolicy::None)
    }

    /// Redact `value` in place according to the policy.
    pub fn redact(&self, value: &mut Value) {
        if !self.is_active() {
            return;
        }
        self.walk_message(value);
    }

    /// Redact `value` in place while preserving primitive JSON schema types
    /// (number -> 0, bool -> false, array -> [], object -> {}, string -> placeholder).
    pub fn redact_preserving_types(&self, value: &mut Value) {
        if !self.is_active() {
            return;
        }
        self.walk_message_typed(value);
    }

    /// Entry point for one full message payload. Recognizes the exact
    /// `tools/list` response shape (`{"result":{"tools":[...]}}`) so the
    /// schema-field exemption in [`Self::walk_tool_definition`] only ever
    /// applies to a genuine tool definition there - never to a same-named
    /// field anywhere else in a payload. A blanket "any key named
    /// `inputSchema`/`outputSchema`" exemption would let a malicious or
    /// buggy server smuggle unredacted data past `--redact default` simply
    /// by naming a field `inputSchema`/`outputSchema` in a `tools/call`
    /// result or any other message.
    fn walk_message(&self, value: &mut Value) {
        let Value::Object(top) = value else {
            self.walk(value);
            return;
        };
        for (key, child) in top.iter_mut() {
            if key == "result" {
                self.walk_result(child);
            } else if self.is_sensitive_key(key) {
                *child = self.placeholder.clone();
            } else {
                self.walk(child);
            }
        }
    }

    fn walk_message_typed(&self, value: &mut Value) {
        let Value::Object(top) = value else {
            self.walk_typed(value);
            return;
        };
        for (key, child) in top.iter_mut() {
            if key == "result" {
                self.walk_result_typed(child);
            } else if self.is_sensitive_key(key) {
                *child = redact_value_typed(child);
            } else {
                self.walk_typed(child);
            }
        }
    }

    /// Walks a JSON-RPC response's `result` object, special-casing its
    /// `tools` array (present only on a `tools/list` response) so each
    /// element is walked as a tool definition. Every other field is walked
    /// generically, same as `walk_message`'s non-`result` fields.
    fn walk_result(&self, value: &mut Value) {
        let Value::Object(result) = value else {
            self.walk(value);
            return;
        };
        for (key, child) in result.iter_mut() {
            if key == "tools" {
                if let Value::Array(tools) = child {
                    for tool in tools.iter_mut() {
                        self.walk_tool_definition(tool);
                    }
                    continue;
                }
            }
            if self.is_sensitive_key(key) {
                *child = self.placeholder.clone();
            } else {
                self.walk(child);
            }
        }
    }

    fn walk_result_typed(&self, value: &mut Value) {
        let Value::Object(result) = value else {
            self.walk_typed(value);
            return;
        };
        for (key, child) in result.iter_mut() {
            if key == "tools" {
                if let Value::Array(tools) = child {
                    for tool in tools.iter_mut() {
                        self.walk_tool_definition_typed(tool);
                    }
                    continue;
                }
            }
            if self.is_sensitive_key(key) {
                *child = redact_value_typed(child);
            } else {
                self.walk_typed(child);
            }
        }
    }

    /// Walks one element of `result.tools[]`. MCP tool schemas are
    /// contracts, not runtime values: a property named `api_key` or `token`
    /// must retain its definition so a redacted recording still describes
    /// the same tool surface and cannot create false drift. This exemption
    /// applies only to this object's own `inputSchema`/`outputSchema`
    /// fields - not recursively, and not anywhere else in the payload.
    fn walk_tool_definition(&self, value: &mut Value) {
        let Value::Object(tool) = value else {
            self.walk(value);
            return;
        };
        for (key, child) in tool.iter_mut() {
            if is_mcp_tool_schema_key(key) {
                continue;
            }
            if self.is_sensitive_key(key) {
                *child = self.placeholder.clone();
            } else {
                self.walk(child);
            }
        }
    }

    fn walk_tool_definition_typed(&self, value: &mut Value) {
        let Value::Object(tool) = value else {
            self.walk_typed(value);
            return;
        };
        for (key, child) in tool.iter_mut() {
            if is_mcp_tool_schema_key(key) {
                continue;
            }
            if self.is_sensitive_key(key) {
                *child = redact_value_typed(child);
            } else {
                self.walk_typed(child);
            }
        }
    }

    /// Generic recursive walk with no exemptions.
    fn walk(&self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    if self.is_sensitive_key(key) {
                        *child = self.placeholder.clone();
                    } else {
                        self.walk(child);
                    }
                }
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    self.walk(item);
                }
            }
            _ => {}
        }
    }

    fn walk_typed(&self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, child) in map.iter_mut() {
                    if self.is_sensitive_key(key) {
                        *child = redact_value_typed(child);
                    } else {
                        self.walk_typed(child);
                    }
                }
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    self.walk_typed(item);
                }
            }
            _ => {}
        }
    }

    fn is_sensitive_key(&self, key: &str) -> bool {
        matches_policy_sensitive_key(key, &self.custom_keys)
    }
}

/// Produces a type-preserving dummy placeholder value for sensitive fields.
/// Preserves numbers, booleans, arrays, objects, and strings as valid JSON primitives
/// so that redacted messages conform to strict JSON schemas during mock replay.
pub fn redact_value_typed(val: &Value) -> Value {
    match val {
        Value::String(_) => Value::String(REDACTED_PLACEHOLDER.to_string()),
        Value::Number(_) => Value::Number(serde_json::Number::from(0)),
        Value::Bool(_) => Value::Bool(false),
        Value::Array(_) => Value::Array(Vec::new()),
        Value::Object(_) => Value::Object(serde_json::Map::new()),
        Value::Null => Value::Null,
    }
}

fn is_mcp_tool_schema_key(key: &str) -> bool {
    matches!(key, "inputSchema" | "outputSchema")
}

/// Lowercase `key` and strip separators so `api_key`, `apiKey`, and `API-KEY`
/// all normalize to `apikey`.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a JSON object key names a value that should be masked.
pub fn is_sensitive_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    if normalized.is_empty() {
        return false;
    }

    if CONTAINS_TOKENS
        .iter()
        .any(|token| normalized.contains(token))
    {
        return true;
    }

    SUFFIX_TOKENS
        .iter()
        .any(|token| normalized == *token || normalized.ends_with(token))
}

/// Return JSON object keys whose values should have been masked by `policy`
/// but are not. Returned keys never include payload values. Tool schema
/// definitions remain exempt because their field names describe contracts, not
/// runtime secrets - but, mirroring [`Redactor::walk_message`], only for a
/// genuine tool definition inside a `tools/list` response's `result.tools[]`,
/// never for a same-named field anywhere else in the payload.
pub fn unredacted_sensitive_keys(value: &Value, policy: &RedactionPolicy) -> Vec<String> {
    if matches!(policy, RedactionPolicy::None) {
        return Vec::new();
    }

    let mut keys = Vec::new();
    collect_unredacted_message(value, policy.custom_keys(), &mut keys);
    keys.sort();
    keys.dedup();
    keys
}

fn matches_policy_sensitive_key(key: &str, custom_keys: &[String]) -> bool {
    if is_sensitive_key(key) {
        return true;
    }

    let normalized = normalize_key(key);
    !normalized.is_empty() && custom_keys.iter().any(|custom| custom == &normalized)
}

fn push_if_unredacted(key: &str, child: &Value, custom_keys: &[String], keys: &mut Vec<String>) {
    if matches_policy_sensitive_key(key, custom_keys) {
        if child.as_str() != Some(REDACTED_PLACEHOLDER) {
            keys.push(key.to_string());
        }
    } else {
        collect_unredacted_sensitive_keys(child, custom_keys, keys);
    }
}

/// Mirrors [`Redactor::walk_message`]: entry point for one full message
/// payload, special-casing a `tools/list` response's `result.tools[]`.
fn collect_unredacted_message(value: &Value, custom_keys: &[String], keys: &mut Vec<String>) {
    let Value::Object(top) = value else {
        collect_unredacted_sensitive_keys(value, custom_keys, keys);
        return;
    };
    for (key, child) in top {
        if key == "result" {
            collect_unredacted_result(child, custom_keys, keys);
        } else {
            push_if_unredacted(key, child, custom_keys, keys);
        }
    }
}

/// Mirrors [`Redactor::walk_result`].
fn collect_unredacted_result(value: &Value, custom_keys: &[String], keys: &mut Vec<String>) {
    let Value::Object(result) = value else {
        collect_unredacted_sensitive_keys(value, custom_keys, keys);
        return;
    };
    for (key, child) in result {
        if key == "tools" {
            if let Value::Array(tools) = child {
                for tool in tools {
                    collect_unredacted_tool_definition(tool, custom_keys, keys);
                }
                continue;
            }
        }
        push_if_unredacted(key, child, custom_keys, keys);
    }
}

/// Mirrors [`Redactor::walk_tool_definition`]: the only place
/// `inputSchema`/`outputSchema` are exempt.
fn collect_unredacted_tool_definition(
    value: &Value,
    custom_keys: &[String],
    keys: &mut Vec<String>,
) {
    let Value::Object(tool) = value else {
        collect_unredacted_sensitive_keys(value, custom_keys, keys);
        return;
    };
    for (key, child) in tool {
        if is_mcp_tool_schema_key(key) {
            continue;
        }
        push_if_unredacted(key, child, custom_keys, keys);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// T-63: Pre-export sensitive-content lint
// ────────────────────────────────────────────────────────────────────────────

/// Category of suspicious content found by [`sensitive_content_lint`].
/// Values are never included — only the category and the JSON pointer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SensitiveCategory {
    /// A string that starts with `Bearer ` followed by a non-trivial token.
    BearerToken,
    /// A URL whose query string contains a key that matches the default
    /// sensitive-key list (e.g. `?api_key=...`).
    UrlQuerySecret,
    /// A PEM block (`-----BEGIN ...-----`) embedded in a string value.
    PemBlock,
    /// A `server_command`-style string whose arguments contain a fragment
    /// that matches the default sensitive-key list (e.g. `--api-key=...`).
    CommandSecret,
}

impl SensitiveCategory {
    /// Stable lowercase label used in error messages and documentation.
    pub fn as_str(&self) -> &'static str {
        match self {
            SensitiveCategory::BearerToken => "bearer_token",
            SensitiveCategory::UrlQuerySecret => "url_query_secret",
            SensitiveCategory::PemBlock => "pem_block",
            SensitiveCategory::CommandSecret => "command_secret",
        }
    }
}

/// A finding from [`sensitive_content_lint`]: a JSON pointer identifying where
/// the suspicious string was found, paired with its category. The actual
/// string value is never included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitiveContentFinding {
    /// JSON Pointer (RFC 6901) to the suspicious string leaf, e.g.
    /// `/params/arguments/prompt`.
    pub pointer: String,
    /// What kind of sensitive pattern was detected.
    pub category: SensitiveCategory,
}

/// Scan every string leaf of `value` for sensitive content that key-name
/// redaction cannot catch: bearer tokens, PEM headers, URL query secrets,
/// and command-embedded secrets.
///
/// Results are returned as `(json_pointer, category)` findings. The actual
/// string values are **never** included — callers may safely print findings
/// in error messages without risking secret exposure.
///
/// Already-redacted placeholders (`***REDACTED***`) are never flagged.
///
/// `command_context` is an extra plain string (e.g. `session.server_command`)
/// that is checked for command-secret patterns but is outside the JSON tree;
/// its finding pointer is `"/session/server_command"`.
pub fn sensitive_content_lint(
    value: &Value,
    command_context: Option<&str>,
) -> Vec<SensitiveContentFinding> {
    let mut findings = Vec::new();
    walk_lint(value, &mut String::from(""), &mut findings);
    if let Some(cmd) = command_context {
        if is_command_secret(cmd) {
            findings.push(SensitiveContentFinding {
                pointer: "/session/server_command".to_string(),
                category: SensitiveCategory::CommandSecret,
            });
        }
    }
    findings.sort_by(|a, b| a.pointer.cmp(&b.pointer).then(a.category.cmp(&b.category)));
    findings.dedup_by(|a, b| a.pointer == b.pointer && a.category == b.category);
    findings
}

fn walk_lint(value: &Value, pointer: &mut String, findings: &mut Vec<SensitiveContentFinding>) {
    match value {
        Value::String(s) => {
            if s == REDACTED_PLACEHOLDER {
                return;
            }
            if is_bearer_token(s) {
                findings.push(SensitiveContentFinding {
                    pointer: pointer.clone(),
                    category: SensitiveCategory::BearerToken,
                });
            }
            if is_pem_block(s) {
                findings.push(SensitiveContentFinding {
                    pointer: pointer.clone(),
                    category: SensitiveCategory::PemBlock,
                });
            }
            if is_url_query_secret(s) {
                findings.push(SensitiveContentFinding {
                    pointer: pointer.clone(),
                    category: SensitiveCategory::UrlQuerySecret,
                });
            }
        }
        Value::Object(map) => {
            let base_len = pointer.len();
            for (key, child) in map {
                pointer.push('/');
                // RFC 6901: escape '~' as '~0' and '/' as '~1' in keys
                let escaped = key.replace('~', "~0").replace('/', "~1");
                pointer.push_str(&escaped);
                walk_lint(child, pointer, findings);
                pointer.truncate(base_len);
            }
        }
        Value::Array(items) => {
            let base_len = pointer.len();
            for (i, item) in items.iter().enumerate() {
                pointer.push('/');
                let idx = i.to_string();
                pointer.push_str(&idx);
                walk_lint(item, pointer, findings);
                pointer.truncate(base_len);
            }
        }
        _ => {}
    }
}

/// A string starting with `Bearer ` followed by at least 8 non-space
/// characters is treated as a bearer credential.
fn is_bearer_token(s: &str) -> bool {
    let lower = s.trim_start();
    if let Some(rest) = lower.strip_prefix("Bearer ") {
        let token = rest.trim_start();
        return token.len() >= 8 && !token.contains('\n');
    }
    // Also case-insensitive prefix check
    let s_lower = s.trim_start().to_ascii_lowercase();
    if let Some(rest) = s_lower.strip_prefix("bearer ") {
        let token = rest.trim_start();
        return token.len() >= 8 && !token.contains('\n');
    }
    false
}

/// A string containing `-----BEGIN ` is a PEM header.
fn is_pem_block(s: &str) -> bool {
    s.contains("-----BEGIN ")
}

/// A URL-like string whose query section contains a key matching the default
/// sensitive-key list.
fn is_url_query_secret(s: &str) -> bool {
    // Find the query string portion: everything after the first '?'
    let Some(query) = s.find('?').map(|i| &s[i + 1..]) else {
        return false;
    };
    // Strip any fragment
    let query = query.split('#').next().unwrap_or("");
    for pair in query.split('&') {
        let key = pair.split('=').next().unwrap_or("").trim();
        if !key.is_empty() && is_sensitive_key(key) {
            return true;
        }
    }
    false
}

/// A command string (space-separated argv) that contains a flag or env-var
/// name whose normalized form matches the default sensitive-key list.
/// Only flags of the form `--key=...` or `-key=...` are checked; plain
/// positional arguments are not flagged.
fn is_command_secret(cmd: &str) -> bool {
    for token in cmd.split_ascii_whitespace() {
        // Strip leading dashes from --api-key or -api-key style flags
        let stripped = token.trim_start_matches('-');
        // Only check `--flag=value` style; bare `--flag` without `=` doesn't
        // embed a value and is not a secret.
        let flag_key = if let Some(key) = stripped.split('=').next() {
            key
        } else {
            continue;
        };
        if stripped.contains('=') && is_sensitive_key(flag_key) {
            return true;
        }
    }
    false
}

/// Generic recursive walk with no exemptions.
fn collect_unredacted_sensitive_keys(
    value: &Value,
    custom_keys: &[String],
    keys: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                push_if_unredacted(key, child, custom_keys, keys);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_unredacted_sensitive_keys(item, custom_keys, keys);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn none_policy_is_a_noop() {
        let redactor = Redactor::new(RedactionPolicy::None);
        assert!(!redactor.is_active());

        let mut value = json!({"password": "hunter2", "keep": "me"});
        let original = value.clone();
        redactor.redact(&mut value);
        assert_eq!(value, original);
    }

    #[test]
    fn masks_common_secret_keys() {
        let redactor = Redactor::new(RedactionPolicy::Default);
        let mut value = json!({
            "password": "hunter2",
            "api_key": "sk-123",
            "accessToken": "abc",
            "authorization": "Bearer xyz",
            "Cookie": "session=1",
            "csrf_token": "t",
        });
        redactor.redact(&mut value);

        for key in [
            "password",
            "api_key",
            "accessToken",
            "authorization",
            "Cookie",
            "csrf_token",
        ] {
            assert_eq!(value[key], json!(REDACTED_PLACEHOLDER), "key {key}");
        }
    }

    #[test]
    fn preserves_non_sensitive_and_structural_keys() {
        let redactor = Redactor::new(RedactionPolicy::Default);
        let mut value = json!({
            "method": "tools/call",
            "id": 7,
            "params": {"name": "read_file", "arguments": {"path": "/tmp/a", "author": "amy"}},
        });
        let original = value.clone();
        redactor.redact(&mut value);
        assert_eq!(value, original, "structural keys must survive redaction");
    }

    #[test]
    fn redacts_nested_and_array_values() {
        let redactor = Redactor::new(RedactionPolicy::Default);
        let mut value = json!({
            "params": {"arguments": {"secret": {"deep": "v"}}},
            "list": [{"token": "a"}, {"ok": "b"}],
        });
        redactor.redact(&mut value);

        assert_eq!(
            value["params"]["arguments"]["secret"],
            json!(REDACTED_PLACEHOLDER)
        );
        assert_eq!(value["list"][0]["token"], json!(REDACTED_PLACEHOLDER));
        assert_eq!(value["list"][1]["ok"], json!("b"));
    }

    #[test]
    fn preserves_mcp_tool_schema_definitions() {
        let redactor = Redactor::new(RedactionPolicy::Default);
        let mut value = json!({
            "result": {
                "tools": [{
                    "name": "configure",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "api_key": {"type": "string"},
                            "token": {"type": "string"}
                        }
                    },
                    "outputSchema": {
                        "type": "object",
                        "properties": {"accessToken": {"type": "string"}}
                    }
                }],
                "runtime": {"api_key": "sk-secret"}
            }
        });
        let input_schema = value["result"]["tools"][0]["inputSchema"].clone();
        let output_schema = value["result"]["tools"][0]["outputSchema"].clone();

        redactor.redact(&mut value);

        assert_eq!(value["result"]["tools"][0]["inputSchema"], input_schema);
        assert_eq!(value["result"]["tools"][0]["outputSchema"], output_schema);
        assert_eq!(value["result"]["runtime"]["api_key"], REDACTED_PLACEHOLDER);
    }

    /// Security regression: a field literally named `inputSchema`/
    /// `outputSchema` must not be a redaction bypass just by its name. The
    /// exemption in `preserves_mcp_tool_schema_definitions` above must apply
    /// only to a genuine tool definition inside `result.tools[]` - a
    /// malicious or buggy server could otherwise smuggle secrets past
    /// `--redact default` by naming a field this way anywhere else.
    #[test]
    fn schema_key_exemption_does_not_apply_outside_a_tool_definition() {
        let redactor = Redactor::new(RedactionPolicy::Default);

        // A tools/call result is not a tools/list response: it has no
        // `result.tools[]`, so a same-named field here is not a tool schema.
        let mut tools_call_result = json!({
            "result": {
                "content": [{"type": "text", "text": "ok"}],
                "outputSchema": {"api_key": "sk-live-secret"}
            }
        });
        redactor.redact(&mut tools_call_result);
        assert_eq!(
            tools_call_result["result"]["outputSchema"]["api_key"],
            REDACTED_PLACEHOLDER
        );

        // Even inside a `tools` array, only the array-element level of an
        // actual result.tools[] entry is exempt - a nested object one level
        // deeper that happens to reuse the name is not.
        let mut nested = json!({
            "result": {
                "tools": [{
                    "name": "configure",
                    "inputSchema": {"type": "object"},
                    "nested": {"inputSchema": {"api_key": "sk-live-secret"}}
                }]
            }
        });
        redactor.redact(&mut nested);
        assert_eq!(
            nested["result"]["tools"][0]["nested"]["inputSchema"]["api_key"],
            REDACTED_PLACEHOLDER
        );

        // A top-level field also named `inputSchema` (no `result` wrapper at
        // all) is never exempt either.
        let mut top_level = json!({"inputSchema": {"api_key": "sk-live-secret"}});
        redactor.redact(&mut top_level);
        assert_eq!(top_level["inputSchema"]["api_key"], REDACTED_PLACEHOLDER);
    }

    #[test]
    fn unredacted_sensitive_keys_flags_schema_key_bypass_outside_a_tool_definition() {
        let policy = RedactionPolicy::Default;

        let leaked = json!({
            "result": {
                "content": [{"type": "text", "text": "ok"}],
                "outputSchema": {"api_key": "sk-live-secret"}
            }
        });
        assert_eq!(
            unredacted_sensitive_keys(&leaked, &policy),
            vec!["api_key".to_string()]
        );

        // The legitimate case must still report nothing.
        let clean = json!({
            "result": {
                "tools": [{
                    "name": "configure",
                    "inputSchema": {"properties": {"api_key": {"type": "string"}}}
                }]
            }
        });
        assert!(unredacted_sensitive_keys(&clean, &policy).is_empty());
    }

    #[test]
    fn policy_round_trips_through_name() {
        assert_eq!(
            RedactionPolicy::from_name("none"),
            Some(RedactionPolicy::None)
        );
        assert_eq!(
            RedactionPolicy::from_name("default"),
            Some(RedactionPolicy::Default)
        );
        assert_eq!(RedactionPolicy::from_name("bogus"), None);
        assert_eq!(RedactionPolicy::Default.as_str(), "default");
    }

    #[test]
    fn custom_keys_augment_default_redaction_with_normalized_matching() {
        let policy = RedactionPolicy::from_cli(
            "default",
            &["tenant-id".to_string(), "customerCode".to_string()],
        )
        .unwrap();
        assert_eq!(policy.as_str(), "custom");

        let redactor = Redactor::new(policy);
        let mut value = json!({
            "tenantId": "tenant-42",
            "customer_code": "customer-7",
            "password": "still-a-default-secret",
            "keep": "visible",
        });
        redactor.redact(&mut value);

        assert_eq!(value["tenantId"], REDACTED_PLACEHOLDER);
        assert_eq!(value["customer_code"], REDACTED_PLACEHOLDER);
        assert_eq!(value["password"], REDACTED_PLACEHOLDER);
        assert_eq!(value["keep"], "visible");
    }

    #[test]
    fn custom_keys_require_the_default_policy() {
        let err = RedactionPolicy::from_cli("none", &["tenant_id".to_string()]).unwrap_err();

        assert_eq!(err, "--redact-keys requires --redact default");
    }

    #[test]
    fn detects_unredacted_values_for_default_and_custom_keys() {
        let policy = RedactionPolicy::from_storage("custom", &["tenant-id".to_string()]).unwrap();
        let value = json!({
            "params": {
                "api_key": "leaked",
                "tenantId": "tenant-42",
                // Not `result.tools[]`, so this is just a params field that
                // happens to be named `inputSchema` - not a real tool
                // definition, so it is not exempt (see
                // `schema_key_exemption_does_not_apply_outside_a_tool_definition`).
                "inputSchema": {"properties": {"token": {"type": "string"}}}
            }
        });

        assert_eq!(
            unredacted_sensitive_keys(&value, &policy),
            vec![
                "api_key".to_string(),
                "tenantId".to_string(),
                "token".to_string()
            ]
        );
    }

    #[test]
    fn accepts_values_masked_for_the_persisted_policy() {
        let policy = RedactionPolicy::from_storage("custom", &["tenant-id".to_string()]).unwrap();
        let value = json!({
            "api_key": REDACTED_PLACEHOLDER,
            "tenantId": REDACTED_PLACEHOLDER,
        });

        assert!(unredacted_sensitive_keys(&value, &policy).is_empty());
    }

    // ── sensitive_content_lint tests (T-63) ──────────────────────────────────

    #[test]
    fn lint_detects_bearer_token_in_string_leaf() {
        let value = json!({"params": {"authorization": "Bearer eyJhbGciOiJSUzI1NiJ9.payload"}});
        let findings = sensitive_content_lint(&value, None);
        assert!(
            findings
                .iter()
                .any(|f| f.category == SensitiveCategory::BearerToken),
            "expected BearerToken finding, got: {findings:?}"
        );
    }

    #[test]
    fn lint_detects_pem_block_in_string_leaf() {
        let value = json!({
            "result": {"content": [{"text": "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK\n-----END RSA PRIVATE KEY-----"}]}
        });
        let findings = sensitive_content_lint(&value, None);
        assert!(
            findings
                .iter()
                .any(|f| f.category == SensitiveCategory::PemBlock),
            "expected PemBlock finding, got: {findings:?}"
        );
    }

    #[test]
    fn lint_detects_url_query_secret() {
        let value =
            json!({"params": {"url": "https://api.example.com/v1?api_key=sk-live-secret&ok=1"}});
        let findings = sensitive_content_lint(&value, None);
        assert!(
            findings
                .iter()
                .any(|f| f.category == SensitiveCategory::UrlQuerySecret),
            "expected UrlQuerySecret finding, got: {findings:?}"
        );
    }

    #[test]
    fn lint_detects_command_secret_in_server_command() {
        let findings =
            sensitive_content_lint(&json!({}), Some("python server.py --api-key=sk-live-prod"));
        assert!(
            findings
                .iter()
                .any(|f| f.category == SensitiveCategory::CommandSecret),
            "expected CommandSecret finding, got: {findings:?}"
        );
    }

    #[test]
    fn lint_reports_json_pointer_not_value() {
        let value = json!({"a": {"b": "Bearer eyJhbGciOiJSUzI1NiJ9.payload"}});
        let findings = sensitive_content_lint(&value, None);
        let bearer = findings
            .iter()
            .find(|f| f.category == SensitiveCategory::BearerToken)
            .expect("expected a BearerToken finding");
        assert_eq!(bearer.pointer, "/a/b");
        for f in &findings {
            assert!(!f.pointer.contains("eyJ"), "pointer must not echo value");
        }
    }

    #[test]
    fn lint_skips_redacted_placeholder() {
        let value = json!({"authorization": REDACTED_PLACEHOLDER});
        let findings = sensitive_content_lint(&value, None);
        assert!(
            findings.is_empty(),
            "redacted placeholder must not be flagged: {findings:?}"
        );
    }

    #[test]
    fn lint_clean_session_returns_empty() {
        let value = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "read_file", "arguments": {"path": "/tmp/test.txt"}}
        });
        let findings = sensitive_content_lint(&value, Some("python server.py --port=8080"));
        assert!(
            findings.is_empty(),
            "clean payload must produce no findings: {findings:?}"
        );
    }

    #[test]
    fn lint_url_without_query_is_not_flagged() {
        let value = json!({"params": {"url": "https://api.example.com/v1/tools"}});
        let findings = sensitive_content_lint(&value, None);
        assert!(
            findings.is_empty(),
            "URL with no query string must not be flagged: {findings:?}"
        );
    }

    #[test]
    fn lint_short_bearer_not_flagged() {
        let value = json!({"x": "Bearer abc"});
        let findings = sensitive_content_lint(&value, None);
        assert!(
            findings.is_empty(),
            "short bearer-like string must not be flagged: {findings:?}"
        );
    }

    #[test]
    fn lint_documents_the_space_separated_flag_gap() {
        // A known, documented false negative (see mtrace-format.md /
        // SECURITY.md): only the `--flag=value` form is checked, not a
        // space-separated `--flag value` pair, since nothing short of
        // guessing ties a bare following token to the preceding flag.
        let findings =
            sensitive_content_lint(&json!({}), Some("python server.py --api-key sk-live-prod"));
        assert!(
            findings.is_empty(),
            "space-separated flag/value is a documented gap, not a detection: {findings:?}"
        );

        let findings =
            sensitive_content_lint(&json!({}), Some("python server.py --api-key=sk-live-prod"));
        assert!(
            findings
                .iter()
                .any(|f| f.category == SensitiveCategory::CommandSecret),
            "the `=`-joined form must still be caught: {findings:?}"
        );
    }

    #[test]
    fn redact_preserving_types_preserves_primitive_schema_types() {
        let redactor = Redactor::new(RedactionPolicy::Default);
        let mut payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "params": {
                "api_key": "sk-live-12345", // gitleaks:allow -- synthetic redaction test fixture, not a credential.
                "secret_number": 42,
                "is_secret": true,
                "access_tokens": ["tok1", "tok2"],
                "credentials": {"user": "admin", "secret_pass": "123"},
                "public_name": "tester"
            }
        });

        redactor.redact_preserving_types(&mut payload);

        let params = payload["params"].as_object().unwrap();
        // Strings become the REDACTED_PLACEHOLDER
        assert_eq!(params["api_key"], json!(REDACTED_PLACEHOLDER));
        // Numbers become 0 (retaining number type)
        assert_eq!(params["secret_number"], json!(0));
        // Booleans become false (retaining boolean type)
        assert_eq!(params["is_secret"], json!(false));
        // Arrays become [] (retaining array type)
        assert_eq!(params["access_tokens"], json!([]));
        // Objects become {} (retaining object type)
        assert_eq!(params["credentials"], json!({}));
        // Non-sensitive fields are untouched
        assert_eq!(params["public_name"], json!("tester"));
    }
}
