//! `list_worktrees`: bounded inventory across repositories with advice.
//!
//! Read-only. Keyset-paginated with at most 20 rows; a page never skips rows
//! and the cursor is scope-checked. The hygiene line counts only what this
//! call already collected; no extra scan is performed.
use crate::response::{self, Class, Templates};
use crate::service::{self, Coverage, Hygiene, ListOutcome, ListRow, Service};
use crate::worktree::{Activity, Integration};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Value, json};

pub const IMPLEMENTED: bool = true;
pub const TEMPLATE: &str = include_str!("../../assets/mcp/tools/list_worktrees.txt.j2");

/// Argument hint used in invalid-arguments refusals.
const ARGS: &str = "Accepted fields: repo?, discovery?, cursor?, limit?, size?.";

/// Typed page view rendered by the embedded template.
#[derive(Serialize)]
struct View {
    status: &'static str,
    returned: usize,
    has_more: bool,
    rows: Vec<RowView>,
    hygiene: String,
    coverage: String,
    cursor: Option<String>,
}

/// One inventory row.
#[derive(Serialize)]
struct RowView {
    key: String,
    class: &'static str,
    branch: String,
    creator: String,
    activity: String,
    integration: String,
    size: String,
    path: String,
}

/// Formats the size axis: exact, lower bound, unmeasured, or not requested.
fn size_label(row: &ListRow, requested: bool) -> String {
    match &row.size {
        Some(size) => {
            let quality = match size.quality {
                crate::worktree::SizeQuality::Complete => "",
                crate::worktree::SizeQuality::LowerBound => " (lower bound)",
            };
            format!("{}{quality}", response::human_bytes(size.bytes))
        }
        None if requested => "unmeasured".to_owned(),
        None => "-".to_owned(),
    }
}

/// Formats the coverage line, naming failed repositories explicitly.
fn coverage_label(coverage: &Coverage) -> String {
    let mut text = format!(
        "{} repos (registry {}, discovered {})",
        coverage.repos, coverage.registry, coverage.discovered
    );
    if !coverage.failed.is_empty() {
        let failed = coverage
            .failed
            .iter()
            .map(|(id, code)| format!("{id}({code})"))
            .collect::<Vec<_>>()
            .join(", ");
        text.push_str(&format!("; failed: {}", response::bounded(&failed, 160)));
    }
    if coverage.budget_exhausted {
        text.push_str("; discovery or call budget exhausted before covering the scope");
    }
    if coverage.orphan_scan_truncated {
        text.push_str("; orphan scan hit its entry cap");
    }
    text
}

/// Mtime-based activity cell: band plus `~`, or `unknown` when the page pass
/// did not finish, or `-` when the row was not probed.
fn activity_cell(row: &ListRow) -> String {
    match row.activity {
        Some(Activity::Unknown) => "unknown".to_owned(),
        Some(band) => format!("{}~", crate::service::activity_label(band)),
        None => "-".to_owned(),
    }
}

/// Mergedness cell: `merged`/`unmerged`/`unknown`, or `-` when not probed.
fn integration_cell(row: &ListRow) -> String {
    match row.integration {
        Some(Integration::AncestorMerged) => "merged".to_owned(),
        Some(Integration::Unmerged) => "unmerged".to_owned(),
        Some(Integration::Unknown) => "unknown".to_owned(),
        None => "-".to_owned(),
    }
}

/// Hygiene line from the rows this page collected.
fn hygiene_label(hygiene: &Hygiene, include_size: bool) -> String {
    let mut text = format!(
        "missing={} idle={} stale={} unmerged={}",
        hygiene.missing, hygiene.idle, hygiene.stale, hygiene.unmerged
    );
    if hygiene.orphan > 0 {
        text.push_str(&format!(" orphan_candidates={}", hygiene.orphan));
    }
    if hygiene.removal_started > 0 {
        text.push_str(&format!(" removal_started={}", hygiene.removal_started));
    }
    if include_size {
        text.push_str(&format!(" large={}", hygiene.large));
    }
    text
}

pub fn definition() -> Value {
    json!({"name":"list_worktrees",
        "description":"List worktrees across the managed scope with ownership classification (managed, foreign, missing, orphan_candidate) and cheap per-row signals: mtime-based activity band (recent~/idle~/stale~) and mergedness. Read-only. Keyset-paginated, at most 20 rows per page; orphan candidates are never automatically deletable.",
        "inputSchema":{"type":"object",
            "properties":{
                "repo":{"type":"string","description":"Restrict the scope to one repository; omit for all known repositories."},
                "discovery":{"type":"boolean","description":"Also scan configured discovery roots; default true."},
                "cursor":{"type":"string","description":"Cursor from the previous page."},
                "limit":{"type":"integer","minimum":1,"maximum":20,"description":"Rows per page; default 20."},
                "size":{"type":"boolean","description":"Measure on-disk size of the rows shown on this page; default false."}},
            "additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}})
}

pub async fn call(args: Value, templates: &Templates, service: &Service) -> CallToolResult {
    let Ok(parsed) = serde_json::from_value::<service::ListArgs>(args) else {
        return response::invalid_arguments(templates, "list_worktrees", ARGS);
    };
    let include_size = parsed.size.unwrap_or(false);
    let outcome = match service.list_worktrees(&parsed).await {
        Ok(outcome) => outcome,
        Err(error) => return response::failure(templates, &error),
    };
    let ListOutcome {
        rows,
        has_more,
        cursor,
        coverage,
        hygiene,
    } = outcome;
    let status = if coverage.failed.is_empty() {
        "OK"
    } else {
        "PARTIAL"
    };
    let view = View {
        status,
        returned: rows.len(),
        has_more,
        rows: rows
            .iter()
            .map(|row| RowView {
                key: row.key.clone(),
                class: row.class_label(),
                branch: row.branch_label().to_owned(),
                creator: row.creator.clone().unwrap_or_else(|| "-".to_owned()),
                activity: activity_cell(row),
                integration: integration_cell(row),
                size: size_label(row, include_size),
                path: row.path.display().to_string(),
            })
            .collect(),
        hygiene: hygiene_label(&hygiene, include_size),
        coverage: coverage_label(&coverage),
        cursor,
    };
    match templates.render("list_worktrees", &view, Class::Page) {
        Ok(text) => response::text_result(text, false),
        // A read whose information could not be presented is an error, never a
        // silently shortened page.
        Err(_) => response::text_result(response::READ_FALLBACK.to_owned(), true),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Test assertions")]
mod tests {
    use super::*;
    use crate::response::Templates;
    use crate::worktree::Size;

    fn templates() -> Templates {
        Templates::new(&[
            ("list_worktrees", TEMPLATE),
            ("error", crate::response::ERROR_TEMPLATE),
            ("outcome_unknown", crate::response::OUTCOME_UNKNOWN_TEMPLATE),
        ])
        .unwrap()
    }

    fn sample_row() -> ListRow {
        ListRow {
            key: "0123456789ab/task-1".to_owned(),
            repo_id: "0".repeat(64),
            path: std::path::PathBuf::from("/tmp/w/demo--0123456789ab/task-1"),
            class: crate::worktree::WorktreeClass::Managed,
            branch: Some("refs/heads/aw/task-1".to_owned()),
            detached: false,
            head: None,
            creator: Some("claude-code".to_owned()),
            removal_started: false,
            activity: Some(crate::worktree::Activity::StaleCandidate),
            integration: Some(crate::worktree::Integration::Unmerged),
            size: Some(Size {
                bytes: 2_u64 << 30,
                quality: crate::worktree::SizeQuality::LowerBound,
            }),
        }
    }

    #[test]
    fn template_fixture() {
        let view = View {
            status: "OK",
            returned: 1,
            has_more: false,
            rows: vec![RowView {
                key: "0123456789ab/task-1".to_owned(),
                class: "managed",
                branch: "refs/heads/aw/task-1".to_owned(),
                creator: "claude-code".to_owned(),
                activity: "stale~".to_owned(),
                integration: "unmerged".to_owned(),
                size: "2.0 GiB (lower bound)".to_owned(),
                path: "/tmp/w/demo--0123456789ab/task-1".to_owned(),
            }],
            hygiene: "missing=0 idle=1 stale=1 unmerged=1 large=true".to_owned(),
            coverage: "1 repos (registry 1, discovered 0)".to_owned(),
            cursor: None,
        };
        let text = templates()
            .render("list_worktrees", &view, Class::Page)
            .unwrap();
        assert_eq!(
            text,
            include_str!("../../tests/fixtures/list_worktrees.txt")
        );
    }

    #[test]
    fn size_and_coverage_labels() {
        let row = sample_row();
        assert_eq!(size_label(&row, true), "2.0 GiB (lower bound)");
        let mut unmeasured = row;
        unmeasured.size = None;
        assert_eq!(size_label(&unmeasured, true), "unmeasured");
        assert_eq!(size_label(&unmeasured, false), "-");
        let coverage = Coverage {
            repos: 3,
            registry: 1,
            discovered: 1,
            failed: vec![("deadbeefcafe".to_owned(), "timeout")],
            budget_exhausted: true,
            orphan_scan_truncated: false,
        };
        let label = coverage_label(&coverage);
        assert!(label.contains("deadbeefcafe(timeout)"));
        assert!(label.contains("budget exhausted"));
    }

    #[tokio::test]
    async fn unknown_field_rejected() {
        let result = call(
            serde_json::json!({"nope": 1}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR invalid_arguments:"), "{text}");
    }

    #[tokio::test]
    async fn limit_out_of_range_refused() {
        let result = call(
            serde_json::json!({"limit": 21}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR limit_out_of_range:"), "{text}");
    }

    #[tokio::test]
    async fn cursor_garbage_refused() {
        let result = call(
            serde_json::json!({"cursor": "!!!!"}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR cursor_invalid:"), "{text}");
    }
}
