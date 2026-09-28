//! `prune_worktrees`: repository-wide registration pruning, dry run by default.
//!
//! External write when `dry_run=false`. Pruning removes stale registrations
//! only — never branches or existing directories. Eligibility is rechecked at
//! apply time because native prune has no exact-entry transaction. The dry run
//! is keyset-paginated (at most 20 candidates per page) so an oversized
//! preview is paged through instead of refusing.
use crate::response::{self, Class, Templates};
use crate::service::{self, PruneOutcome, Service};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Value, json};

pub const IMPLEMENTED: bool = true;
pub const TEMPLATE: &str = include_str!("../../assets/mcp/tools/prune_worktrees.txt.j2");

/// Argument hint used in invalid-arguments refusals.
const ARGS: &str = "Accepted fields: repo, dry_run?, cursor?, limit?.";

/// Typed page view rendered by the embedded template.
#[derive(Serialize)]
struct View {
    status: &'static str,
    repo: String,
    total: usize,
    shown: usize,
    has_more: bool,
    applied: bool,
    rows: Vec<String>,
    cursor: Option<String>,
    warnings: Option<String>,
}

pub fn definition() -> Value {
    json!({"name":"prune_worktrees",
        "description":"Prune stale worktree registrations for one repository. dry_run=true (the default) lists missing, unlocked registrations page by page (keyset cursor, at most 20 paths per page); dry_run=false runs native git worktree prune repository-wide, which removes registrations only, never branches or existing directories.",
        "inputSchema":{"type":"object","required":["repo"],
            "properties":{
                "repo":{"type":"string","description":"Repository root or any worktree inside it."},
                "dry_run":{"type":"boolean","description":"List candidates only; default true."},
                "cursor":{"type":"string","description":"Cursor from the previous preview page."},
                "limit":{"type":"integer","minimum":1,"maximum":20,"description":"Candidate paths per preview page; default 20."}},
            "additionalProperties":false},
        "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}})
}

pub async fn call(args: Value, templates: &Templates, service: &Service) -> CallToolResult {
    let Ok(parsed) = serde_json::from_value::<service::PruneArgs>(args) else {
        return response::invalid_arguments(templates, "prune_worktrees", ARGS);
    };
    let PruneOutcome {
        repo_id,
        candidates,
        total,
        has_more,
        cursor,
        applied,
        warnings,
    } = match service.prune_worktrees(&parsed).await {
        Ok(outcome) => outcome,
        Err(error) => return response::failure(templates, &error),
    };
    let view = View {
        status: if applied { "COMMITTED" } else { "OK" },
        repo: repo_id,
        total,
        shown: candidates.len(),
        has_more,
        applied,
        rows: candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        cursor,
        warnings: response::join_warnings(&warnings),
    };
    match templates.render("prune_worktrees", &view, Class::Page) {
        Ok(text) => response::text_result(text, false),
        // A dry-run read that cannot be presented is an error; an applied
        // prune is a confirmed mutation whose receipt must survive.
        Err(_) if applied => response::text_result(
            format!(
                "COMMITTED prune_worktrees {}: {} registrations pruned\n\
Presentation: degraded (presentation_failed).\n\
Do not repeat the prune to repair this response.\n",
                view.repo, view.total
            ),
            false,
        ),
        Err(_) => response::text_result(response::READ_FALLBACK.to_owned(), true),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Test assertions")]
mod tests {
    use super::*;
    use crate::response::Templates;

    fn templates() -> Templates {
        Templates::new(&[
            ("prune_worktrees", TEMPLATE),
            ("error", crate::response::ERROR_TEMPLATE),
            ("outcome_unknown", crate::response::OUTCOME_UNKNOWN_TEMPLATE),
        ])
        .unwrap()
    }

    #[test]
    fn template_fixture() {
        let view = View {
            status: "OK",
            repo: "0123456789ab".to_owned(),
            total: 1,
            shown: 1,
            has_more: false,
            applied: false,
            rows: vec!["/tmp/w/demo--0123456789ab/old-task".to_owned()],
            cursor: None,
            warnings: None,
        };
        let text = templates()
            .render("prune_worktrees", &view, Class::Page)
            .unwrap();
        assert_eq!(
            text,
            include_str!("../../tests/fixtures/prune_worktrees.txt")
        );
    }

    #[test]
    fn preview_page_carries_cursor_and_totals() {
        let view = View {
            status: "OK",
            repo: "0123456789ab".to_owned(),
            total: 25,
            shown: 20,
            has_more: true,
            applied: false,
            rows: vec!["/tmp/w/demo--0123456789ab/old-task".to_owned()],
            cursor: Some("awprune1-cursor".to_owned()),
            warnings: None,
        };
        let text = templates()
            .render("prune_worktrees", &view, Class::Page)
            .unwrap();
        assert!(text.contains("25 candidates; showing 20"), "{text}");
        assert!(text.contains("Cursor: awprune1-cursor"), "{text}");
    }

    #[tokio::test]
    async fn unknown_field_rejected() {
        let result = call(
            serde_json::json!({"repo":"/repo","dry_run":true,"nope":1}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR invalid_arguments:"), "{text}");
    }
}
