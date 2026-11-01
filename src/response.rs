//! Closed embedded templates for product tools. Only typed views cross this boundary.
//!
//! Rendering is bounded per response class (ack 2 KiB, entity 4 KiB, page 8 KiB
//! per `docs/MCP_RESPONSE_STANDARD.md`): an oversize render fails and is refused
//! or replaced by a Rust-side receipt, never truncated.
use crate::service::{ServiceError, ServiceOutcome};
use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Serialize;
use std::io::{self, Write};

/// Response class selecting the per-response hard byte cap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// Mutation acknowledgement or routine error.
    Ack,
    /// Compact entity.
    Entity,
    /// Search/list page.
    Page,
}

impl Class {
    /// Hard UTF-8 byte cap for rendered text of this class.
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::Ack => 2 * 1024,
            Self::Entity => 4 * 1024,
            Self::Page => 8 * 1024,
        }
    }
}

/// Shared refusal form: `ERROR <code>: <message>` plus one optional next step.
pub const ERROR_TEMPLATE: &str =
    "ERROR {{ code }}: {{ message }}\n{% if next %}Next: {{ next }}\n{% endif %}";

/// Unknown-effect form for mutations whose confirmation was lost.
pub const OUTCOME_UNKNOWN_TEMPLATE: &str = "OUTCOME_UNKNOWN {{ code }}: {{ target }}\n\
The effect may have been applied; confirmation was not received.\n\
Next: inspect {{ path }} and reconcile before any retry.\n";

/// Static fallback when even the error templates cannot render.
const FALLBACK_ERROR: &str = "ERROR presentation_failed: no effect was performed.\n";
/// Static fallback when a read result could not be presented.
pub const READ_FALLBACK: &str = "ERROR presentation_failed: the read-only result could not be formatted.\n\
Next: narrow the request; no rows were shown.\n";

/// View for [`ERROR_TEMPLATE`].
#[derive(Serialize)]
pub struct ErrorView {
    /// Stable snake_case error code.
    pub code: String,
    /// Bounded human-readable message.
    pub message: String,
    /// Optional single safe recovery step.
    pub next: Option<String>,
}

/// View for [`OUTCOME_UNKNOWN_TEMPLATE`].
#[derive(Serialize)]
pub struct OutcomeUnknownView {
    /// Stable snake_case error code.
    pub code: String,
    /// Operation and target identity.
    pub target: String,
    /// Exact path to inspect for reconciliation.
    pub path: String,
}

pub struct Templates {
    env: Environment<'static>,
}
impl Templates {
    /// Registers the closed template set; a parse failure fails startup.
    pub fn new(entries: &[(&'static str, &'static str)]) -> Result<Self, &'static str> {
        let mut env = Environment::empty();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        env.set_auto_escape_callback(|_| AutoEscape::None);
        env.set_trim_blocks(true);
        env.set_lstrip_blocks(true);
        env.set_keep_trailing_newline(true);
        env.set_fuel(Some(50_000));
        env.set_recursion_limit(16);
        env.set_formatter(mcp_presentation::json_bool_formatter);
        for (name, source) in entries {
            env.add_template(name, source)
                .map_err(|_| "template_invalid")?;
        }
        Ok(Self { env })
    }

    /// Renders one view under its class cap; oversize fails, never truncates.
    pub fn render<T: Serialize>(
        &self,
        name: &str,
        view: &T,
        class: Class,
    ) -> Result<String, &'static str> {
        let mut writer = Limit(Vec::new(), class.bytes());
        self.env
            .get_template(name)
            .map_err(|_| "template_missing")?
            .render_captured_to(view, &mut writer)
            .map_err(|_| "presentation_failed")?;
        String::from_utf8(writer.0).map_err(|_| "presentation_encoding")
    }
}
struct Limit(Vec<u8>, usize);
impl Write for Limit {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > self.1 {
            return Err(io::Error::other("response limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Wraps final text with an explicit execution-error flag.
#[must_use]
pub fn text_result(text: String, is_error: bool) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.is_error = Some(is_error);
    result
}

/// Outer-envelope version of the additive machine-readable read contract
/// (`schema_version` in `structuredContent`). Bumped only by a compatibility
/// review, never by a layout-only change.
pub const READ_SCHEMA_VERSION: u32 = 1;

/// Bounded structured failure outcome; mirrors [`ErrorView`] without prose
/// authority. Carries a stable code, the execution outcome class and a bounded
/// detail — never raw config, process arguments or credentials.
#[derive(Serialize)]
pub struct ReadErrorDto {
    /// Stable snake_case error code, identical to the one in the text form.
    pub code: String,
    /// Execution outcome class: `blocked`, `unavailable` or `outcome_unknown`.
    pub outcome: &'static str,
    /// Bounded human-readable detail (already capped by the service layer).
    pub detail: String,
    /// Optional single safe recovery step, or `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Stable label for a service outcome class, shared by every structured error.
#[must_use]
pub fn outcome_label(outcome: ServiceOutcome) -> &'static str {
    match outcome {
        ServiceOutcome::Blocked => "blocked",
        ServiceOutcome::Unavailable => "unavailable",
        ServiceOutcome::OutcomeUnknown => "outcome_unknown",
    }
}

/// Typed outer envelope of the read contract: `schema_version` and `kind`
/// always, then exactly one of `data` (successful read) or `error` (typed
/// failure); the absent branch is omitted, never a misleading `null`.
#[derive(Serialize)]
struct ReadEnvelope<'a> {
    schema_version: u32,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ReadErrorDto>,
}

/// Builds the outer read envelope for `structuredContent`. A failed read is
/// therefore never representable as an empty success.
#[must_use]
pub fn read_envelope(
    kind: &str,
    data: Option<serde_json::Value>,
    error: Option<ReadErrorDto>,
) -> serde_json::Value {
    serde_json::to_value(ReadEnvelope {
        schema_version: READ_SCHEMA_VERSION,
        kind,
        data,
        error,
    })
    .unwrap_or_else(|_| serde_json::json!({"schema_version": READ_SCHEMA_VERSION, "kind": kind}))
}

/// Attaches structured content conforming to the tool's declared
/// `outputSchema`; the single text block and `isError` stay unchanged.
#[must_use]
pub fn with_structured(
    mut result: CallToolResult,
    structured: serde_json::Value,
) -> CallToolResult {
    result.structured_content = Some(structured);
    result
}

/// Builds a tool `outputSchema` around one tool-specific data schema: the
/// outer read envelope (`schema_version`, `kind`, exactly one of `data` or
/// `error`), so a failed read conforms through the `error` branch. A `$defs`
/// object nested in the data schema is hoisted to the document root, because
/// `#/$defs/…` references resolve from there.
#[must_use]
pub fn read_schema(kind: &str, data: &serde_json::Value) -> serde_json::Value {
    let mut schema = serde_json::json!({
        "type":"object",
        "required":["schema_version","kind"],
        "properties":{
            "schema_version":{"type":"integer","const":READ_SCHEMA_VERSION},
            "kind":{"type":"string","const":kind},
            "data": data,
            "error":{
                "type":"object","required":["code","outcome","detail"],
                "additionalProperties":false,
                "properties":{
                    "code":{"type":"string"},
                    "outcome":{"type":"string","enum":["blocked","unavailable","outcome_unknown"]},
                    "detail":{"type":"string"},
                    "next":{"type":"string"}}}},
        "additionalProperties":false,
        "oneOf":[{"required":["data"]},{"required":["error"]}]
    });
    if let Some(defs) = data.get("$defs") {
        schema["$defs"] = defs.clone();
        if let Some(data_schema) = schema["properties"]["data"].as_object_mut() {
            data_schema.remove("$defs");
        }
    }
    schema
}

/// Structured sibling of [`invalid_arguments`]: the same text form plus the
/// `invalid_arguments` error outcome under the read envelope.
#[must_use]
pub fn read_invalid_arguments(
    templates: &Templates,
    tool: &str,
    hint: &str,
    kind: &str,
) -> CallToolResult {
    let result = invalid_arguments(templates, tool, hint);
    let error = ReadErrorDto {
        code: "invalid_arguments".to_owned(),
        outcome: "blocked",
        detail: bounded(&format!("{tool} rejected the arguments. {hint}"), 300),
        next: None,
    };
    with_structured(result, read_envelope(kind, None, Some(error)))
}

/// Structured sibling of [`failure`]: the same text form plus the typed error
/// outcome (code, class, bounded detail) under the read envelope.
#[must_use]
pub fn read_failure(templates: &Templates, kind: &str, error: &ServiceError) -> CallToolResult {
    let result = failure(templates, error);
    let dto = ReadErrorDto {
        code: error.code.clone(),
        outcome: outcome_label(error.outcome),
        detail: error.detail.clone(),
        next: error.next.clone(),
    };
    with_structured(result, read_envelope(kind, None, Some(dto)))
}

/// Typed projection of one [`crate::worktree::Probe`] axis: the independent
/// probe state plus the typed value, or the stable failure code, or the
/// partial-evidence reason. Exactly the fields matching `state` are filled;
/// the others serialize as `null` so the shape never varies.
#[derive(Serialize)]
pub struct ProbeDto {
    /// `known`, `not_checked`, `unavailable` or `incomplete`.
    pub state: &'static str,
    /// The value when known, the guaranteed partial evidence when
    /// incomplete, otherwise `null`.
    pub value: Option<serde_json::Value>,
    /// Stable reason code when unavailable, otherwise `null`.
    pub code: Option<String>,
    /// Bounded reason when incomplete, otherwise `null`.
    pub reason: Option<String>,
}

impl ProbeDto {
    /// Projects one probe through `value`, which converts the typed evidence
    /// (including [`crate::worktree::Probe::Incomplete`] evidence) to JSON.
    pub fn from_probe<T>(
        probe: &crate::worktree::Probe<T>,
        value: impl Fn(&T) -> serde_json::Value,
    ) -> Self {
        match probe {
            crate::worktree::Probe::Known(known) => Self {
                state: "known",
                value: Some(value(known)),
                code: None,
                reason: None,
            },
            crate::worktree::Probe::NotChecked => Self {
                state: "not_checked",
                value: None,
                code: None,
                reason: None,
            },
            crate::worktree::Probe::Unavailable { code } => Self {
                state: "unavailable",
                value: None,
                code: Some(bounded(code, 120)),
                reason: None,
            },
            crate::worktree::Probe::Incomplete { evidence, reason } => Self {
                state: "incomplete",
                value: Some(value(evidence)),
                code: None,
                reason: Some(bounded(reason, 200)),
            },
        }
    }
}

/// Structured form of one advisory warning: a stable code plus the bounded
/// numeric or textual facts it carries, without human-formatted labels.
#[must_use]
pub fn warning_value(warning: &crate::worktree::Warning) -> serde_json::Value {
    use crate::worktree::Warning;
    match warning {
        Warning::SizeAtLeast { bytes } => {
            serde_json::json!({"code": "size_at_least", "bytes": bytes})
        }
        Warning::Unmerged => serde_json::json!({"code": "unmerged"}),
        Warning::IntegrationUnknown => serde_json::json!({"code": "integration_unknown"}),
        Warning::ProbeIncomplete { reason } => {
            serde_json::json!({"code": "probe_incomplete", "reason": bounded(reason, 200)})
        }
        Warning::RemovalStarted => serde_json::json!({"code": "removal_started"}),
        Warning::ResumableDeletion => serde_json::json!({"code": "resumable_deletion"}),
        Warning::ResumedRemoval => serde_json::json!({"code": "resumed_removal"}),
        Warning::RecordMismatch => serde_json::json!({"code": "record_mismatch"}),
    }
}

/// First text block of a reply; empty when the reply has no text block.
#[cfg(test)]
#[must_use]
pub fn first_text(result: &CallToolResult) -> String {
    match result.content.first() {
        Some(ContentBlock::Text(block)) => block.text.clone(),
        _ => String::new(),
    }
}

/// Rejects argument decoding or validation; no effect was performed.
#[must_use]
pub fn invalid_arguments(templates: &Templates, tool: &str, hint: &str) -> CallToolResult {
    let view = ErrorView {
        code: "invalid_arguments".to_owned(),
        message: bounded(&format!("{tool} rejected the arguments. {hint}"), 300),
        next: None,
    };
    let text = templates
        .render("error", &view, Class::Ack)
        .unwrap_or_else(|_| FALLBACK_ERROR.to_owned());
    text_result(text, true)
}

/// Renders a typed service failure, preserving the unknown-effect form.
#[must_use]
pub fn failure(templates: &Templates, error: &ServiceError) -> CallToolResult {
    if error.outcome == ServiceOutcome::OutcomeUnknown {
        let unknown = error.unknown.as_ref();
        let view = OutcomeUnknownView {
            code: error.code.clone(),
            target: unknown.map_or_else(String::new, |u| u.target.clone()),
            path: unknown.map_or_else(String::new, |u| u.path.clone()),
        };
        let text = templates
            .render("outcome_unknown", &view, Class::Ack)
            .unwrap_or_else(|_| {
                format!(
                    "OUTCOME_UNKNOWN {}: {}\nNext: inspect {} and reconcile before any retry.\n",
                    error.code, error.detail, view.path
                )
            });
        return text_result(text, true);
    }
    let view = ErrorView {
        code: error.code.clone(),
        message: error.detail.clone(),
        next: error.next.clone(),
    };
    let text = templates
        .render("error", &view, Class::Ack)
        .unwrap_or_else(|_| FALLBACK_ERROR.to_owned());
    text_result(text, true)
}

/// Shortens a display string on a char boundary with an explicit marker.
#[must_use]
pub fn bounded(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut cut = max_bytes.saturating_sub(3);
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &text[..cut])
}

/// Formats a byte count as a compact binary-unit label.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Drops the `refs/heads/` prefix from a branch ref for display.
///
/// The full ref stays authoritative everywhere else; this is rendering only.
#[must_use]
pub fn short_branch(branch: &str) -> &str {
    branch.strip_prefix("refs/heads/").unwrap_or(branch)
}

/// Formats unix seconds as UTC ISO-8601 (`2027-01-15T08:00:00Z`).
#[must_use]
pub fn iso_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let day_secs = secs % 86_400;
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let doe = (shifted - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        day_secs / 3600,
        day_secs / 60 % 60,
        day_secs % 60
    )
}

/// Joins stable warning codes into one bounded display line.
#[must_use]
pub fn join_warnings(warnings: &[String]) -> Option<String> {
    if warnings.is_empty() {
        None
    } else {
        Some(bounded(&warnings.join("; "), 240))
    }
}

/// One advisory warning as a stable display label.
#[must_use]
pub fn warning_label(warning: &crate::worktree::Warning) -> String {
    use crate::worktree::Warning;
    match warning {
        Warning::SizeAtLeast { bytes } => format!("size_at_least {}", human_bytes(*bytes)),
        Warning::Unmerged => "unmerged".to_owned(),
        Warning::IntegrationUnknown => "integration_unknown".to_owned(),
        Warning::ProbeIncomplete { reason } => {
            format!("probe_incomplete ({})", bounded(reason, 80))
        }
        Warning::RemovalStarted => "removal_started".to_owned(),
        Warning::ResumableDeletion => {
            "resumable_deletion (set resume_interrupted=true to finish an interrupted \
             removal; without it these deletions stay pending work)"
                .to_owned()
        }
        Warning::ResumedRemoval => "resumed_removal".to_owned(),
        Warning::RecordMismatch => "record_mismatch".to_owned(),
    }
}

/// Joins typed warnings into one bounded display line.
#[must_use]
pub fn warning_line(warnings: &[crate::worktree::Warning]) -> Option<String> {
    let labels: Vec<String> = warnings.iter().map(warning_label).collect();
    join_warnings(&labels)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Test assertions")]
mod tests {
    use super::*;

    #[test]
    fn strict_missing_rejected() {
        let r = Templates::new(&[("test", "{{ missing }}")]).unwrap();
        assert!(r.render("test", &(), Class::Page).is_err());
    }

    #[test]
    fn caps_are_per_class() {
        let r = Templates::new(&[("test", "{{ data }}")]).unwrap();
        let view = serde_json::json!({"data": "x".repeat(2100)});
        assert!(r.render("test", &view, Class::Ack).is_err());
        assert!(r.render("test", &view, Class::Entity).is_ok());
        let big = serde_json::json!({"data": "x".repeat(4100)});
        assert!(r.render("test", &big, Class::Entity).is_err());
        assert!(r.render("test", &big, Class::Page).is_ok());
        let huge = serde_json::json!({"data": "x".repeat(8193)});
        assert!(r.render("test", &huge, Class::Page).is_err());
    }

    #[test]
    fn error_template_shape() {
        let r = Templates::new(&[("error", ERROR_TEMPLATE)]).unwrap();
        let view = ErrorView {
            code: "conflict".to_owned(),
            message: "directory exists without a matching record".to_owned(),
            next: Some("inspect the existing worktree, then choose another name.".to_owned()),
        };
        let text = r.render("error", &view, Class::Ack).unwrap();
        assert_eq!(
            text,
            "ERROR conflict: directory exists without a matching record\n\
             Next: inspect the existing worktree, then choose another name.\n"
        );
    }

    #[test]
    fn bounded_shortens_on_char_boundary() {
        assert_eq!(bounded("abcdef", 6), "abcdef");
        let long = "é".repeat(100);
        let cut = bounded(&long, 10);
        assert!(cut.len() <= 10 && cut.ends_with("..."));
    }

    #[test]
    fn read_schema_hoists_defs_and_resolves_refs() {
        let data = serde_json::json!({
            "type":"object","required":["rows"],"additionalProperties":false,
            "properties":{"rows":{"type":"array","items":{"$ref":"#/$defs/row"}}},
            "$defs":{
                "row":{"type":"object","required":["key"],"additionalProperties":false,
                    "properties":{"key":{"type":"string"},"inner":{"$ref":"#/$defs/inner"}}},
                "inner":{"type":"string"}}}
        );
        let schema = read_schema("worktree_page", &data);
        // `$defs` lives at the document root, where `#/$defs/…` resolves.
        assert!(schema.get("$defs").is_some());
        assert!(schema["properties"]["data"].get("$defs").is_none());
        assert_eq!(schema["properties"]["kind"]["const"], "worktree_page");
        assert_eq!(schema["properties"]["schema_version"]["const"], 1);
        fn check(node: &serde_json::Value, root: &serde_json::Value) {
            match node {
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::String(reference)) = map.get("$ref") {
                        let target = reference.strip_prefix("#/").unwrap();
                        let mut current = root;
                        for part in target.split('/') {
                            let part = part.replace("~1", "/").replace("~0", "~");
                            current = &current[part.as_str()];
                            assert!(!current.is_null(), "dangling {reference}");
                        }
                    }
                    for value in map.values() {
                        check(value, root);
                    }
                }
                serde_json::Value::Array(items) => {
                    for value in items {
                        check(value, root);
                    }
                }
                _ => {}
            }
        }
        check(&schema, &schema);
    }
}
