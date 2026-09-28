//! `prune_worktrees`: repository-wide registration pruning, dry run by default.
//!
//! External write when `dry_run=false`. Pruning removes stale registrations
//! only — never branches or existing directories. Eligibility is rechecked at
//! apply time because native prune has no exact-entry transaction. More than
//! 20 candidates refuses the listing instead of silently skipping rows.
use crate::response::{self, Class, Templates};
use crate::service::{self, PruneOutcome, Service};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Value, json};

pub const IMPLEMENTED: bool = true;
pub const TEMPLATE: &str = include_str!("../../assets/mcp/tools/prune_worktrees.txt.j2");

/// Argument hint used in invalid-arguments refusals.
const ARGS: &str = "Accepted fields: repo, dry_run?.";

/// Typed page view rendered by the embedded template.
#[derive(Serialize)]
struct View {
    status: &'static str,
    repo: String,
    count: usize,
    applied: bool,
    rows: Vec<String>,
    listing_refused: bool,
    warnings: Option<String>,
    total: usize,
}

/// Maximum candidate paths listed on one page.
const MAX_LISTED: usize = 20;

pub fn definition() -> Value {
    json!({"name":"prune_worktrees",
        "description":"Prune stale worktree registrations for one repository. dry_run=true (the default) lists missing, unlocked registrations; dry_run=false runs native git worktree prune, which removes registrations only, never branches or existing directories.",
        "inputSchema":{"type":"object","required":["repo"],
            "properties":{
                "repo":{"type":"string","description":"Repository root or any worktree inside it."},
                "dry_run":{"type":"boolean","description":"List candidates only; default true."}},
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
        applied,
        warnings,
    } = match service.prune_worktrees(&parsed).await {
        Ok(outcome) => outcome,
        Err(error) => return response::failure(templates, &error),
    };
    let total = candidates.len();
    // Never skip unseen rows: an oversized candidate list refuses the listing.
    let (rows, listing_refused) = if total > MAX_LISTED {
        (Vec::new(), true)
    } else {
        (
            candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            false,
        )
    };
    let view = View {
        status: if applied { "COMMITTED" } else { "OK" },
        repo: repo_id,
        count: total,
        applied,
        rows,
        listing_refused,
        warnings: response::join_warnings(&warnings),
        total,
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
                view.repo, view.count
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
            count: 1,
            applied: false,
            rows: vec!["/tmp/w/demo--0123456789ab/old-task".to_owned()],
            listing_refused: false,
            warnings: None,
            total: 1,
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
    fn oversized_candidate_list_refuses_listing() {
        let view = View {
            status: "OK",
            repo: "0123456789ab".to_owned(),
            count: 25,
            applied: false,
            rows: Vec::new(),
            listing_refused: true,
            warnings: None,
            total: 25,
        };
        let text = templates()
            .render("prune_worktrees", &view, Class::Page)
            .unwrap();
        assert!(text.contains("refused"));
        assert!(!text.contains("/tmp/w/"));
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
