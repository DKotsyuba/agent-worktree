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
}
