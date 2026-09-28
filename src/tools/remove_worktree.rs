//! `remove_worktree`: preview → fingerprint → apply removal, one worktree at a time.
//!
//! External write. Preview assesses without effect and returns the fingerprint;
//! apply re-observes under the repository lock and refuses on any veto,
//! divergence or unknown probe. Git is never passed `--force` and the branch is
//! always retained. The apply receipt survives a rendering failure from Rust.
use crate::response::{self, Class, Templates};
use crate::service::{self, RemoveOutcome, RemoveOutcomeKind, Service};
use crate::worktree::{Decision, Veto};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Value, json};

pub const IMPLEMENTED: bool = true;
pub const TEMPLATE: &str = include_str!("../../assets/mcp/tools/remove_worktree.txt.j2");
/// Acknowledgement form used by apply mode.
pub const RECEIPT_TEMPLATE: &str =
    include_str!("../../assets/mcp/tools/remove_worktree_receipt.txt.j2");

/// Argument hint used in invalid-arguments refusals.
const ARGS: &str = "Accepted fields: repo, name?|path?, mode (preview|apply), \
disposable_paths?, allow_unmerged?, fingerprint? (apply).";

/// Typed preview view rendered by the embedded template.
#[derive(Serialize)]
struct PreviewView {
    key: String,
    path: String,
    head: String,
    branch: String,
    disposable: String,
    eligible: bool,
    veto_count: usize,
    vetoes: Vec<VetoView>,
    warnings: Option<String>,
    fingerprint: String,
    next: String,
}

/// One blocking veto.
#[derive(Serialize)]
struct VetoView {
    code: &'static str,
    detail: String,
}

/// Typed apply receipt.
#[derive(Serialize)]
struct ReceiptView {
    status: &'static str,
    key: String,
    path: String,
    branch: String,
    warnings: Option<String>,
}

/// One veto as a view, with detail only where it carries facts.
fn veto_view(veto: &Veto) -> VetoView {
    match veto {
        Veto::IgnoredNotDisposable { paths } => VetoView {
            code: veto.code(),
            detail: format!(" ({})", response::bounded(&paths.join(", "), 160)),
        },
        _ => VetoView {
            code: veto.code(),
            detail: String::new(),
        },
    }
}

/// Rust-side receipt used when the template fails after a confirmed removal.
fn degraded(status: &str, key: &str, path: &str) -> String {
    format!(
        "{status} remove_worktree {key}\nPath: {path} (removed)\n\
Presentation: degraded (presentation_failed).\n\
Do not repeat the removal to repair this response.\n"
    )
}

fn preview_view(
    key: String,
    path: &std::path::Path,
    decision: &Decision,
    disposable: &[std::path::PathBuf],
    head: &Option<String>,
    branch: &Option<String>,
) -> PreviewView {
    let eligible = decision.vetoes.is_empty();
    PreviewView {
        key,
        path: path.display().to_string(),
        head: head.clone().unwrap_or_else(|| "unknown".to_owned()),
        branch: branch
            .as_deref()
            .map(response::short_branch)
            .unwrap_or("detached")
            .to_owned(),
        disposable: if disposable.is_empty() {
            "(none)".to_owned()
        } else {
            response::bounded(
                &disposable
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                512,
            )
        },
        eligible,
        veto_count: decision.vetoes.len(),
        vetoes: decision.vetoes.iter().map(veto_view).collect(),
        warnings: response::warning_line(&decision.warnings),
        fingerprint: decision
            .fingerprint
            .as_ref()
            .map_or_else(|| "-".to_owned(), |f| f.as_str().to_owned()),
        next: if eligible {
            "call remove_worktree mode=apply with the same target, disposable_paths, \
allow_unmerged and this fingerprint."
                .to_owned()
        } else {
            "resolve the vetoes above, then rerun the preview.".to_owned()
        },
    }
}

pub fn definition() -> Value {
    json!({"name":"remove_worktree",
        "description":"Remove one worktree. mode=preview assesses without effect and returns a fingerprint plus vetoes; mode=apply removes it under that fingerprint. Never forces and never deletes the branch; dirty, locked, protected or unverified trees are refused. An interrupted apply stays visible through the record's removal_started marker.",
        "inputSchema":{"type":"object","required":["repo","mode"],
            "properties":{
                "repo":{"type":"string","description":"Repository root or any worktree inside it."},
                "name":{"type":"string","description":"Worktree directory name; provide name or path, not both."},
                "path":{"type":"string","description":"Absolute worktree path; provide name or path, not both."},
                "mode":{"type":"string","enum":["preview","apply"],"description":"preview assesses; apply removes under the preview fingerprint."},
                "disposable_paths":{"type":"array","maxItems":64,"items":{"type":"string","maxLength":256},"description":"Ignored paths approved for deletion, worktree-relative (for example target/)."},
                "allow_unmerged":{"type":"boolean","description":"Explicitly allow removing an unmerged worktree; the branch is retained."},
                "fingerprint":{"type":"string","maxLength":64,"description":"Fingerprint returned by the preview being applied; required for apply."}},
            "additionalProperties":false},
        "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}})
}

pub async fn call(args: Value, templates: &Templates, service: &Service) -> CallToolResult {
    let Ok(parsed) = serde_json::from_value::<service::RemoveArgs>(args) else {
        return response::invalid_arguments(templates, "remove_worktree", ARGS);
    };
    match service.remove_worktree(&parsed).await {
        Ok(RemoveOutcome::Preview {
            key,
            path,
            head,
            branch,
            disposable,
            decision,
        }) => {
            let view = preview_view(key, &path, &decision, &disposable, &head, &branch);
            match templates.render("remove_worktree", &view, Class::Entity) {
                Ok(text) => response::text_result(text, false),
                Err(_) => response::text_result(response::READ_FALLBACK.to_owned(), true),
            }
        }
        Ok(RemoveOutcome::Applied {
            key,
            path,
            branch,
            outcome,
            warnings,
        }) => {
            let (status, suffix) = match outcome {
                RemoveOutcomeKind::Removed => ("COMMITTED", "removed"),
                RemoveOutcomeKind::AlreadyAbsent => ("NOOP", "already absent"),
            };
            let path_text = path.display().to_string();
            let view = ReceiptView {
                status,
                key: key.clone(),
                path: format!("{path_text} ({suffix})"),
                branch: branch
                    .map(|b| format!("retained ({})", response::short_branch(&b)))
                    .unwrap_or_else(|| "detached".to_owned()),
                warnings: response::join_warnings(&warnings),
            };
            match templates.render("remove_worktree_receipt", &view, Class::Ack) {
                Ok(text) => response::text_result(text, false),
                Err(_) => response::text_result(degraded(status, &key, &path_text), false),
            }
        }
        Err(error) => response::failure(templates, &error),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Test assertions")]
mod tests {
    use super::*;
    use crate::response::Templates;
    use crate::worktree::{Fingerprint, Warning};

    fn templates() -> Templates {
        Templates::new(&[
            ("remove_worktree", TEMPLATE),
            ("remove_worktree_receipt", RECEIPT_TEMPLATE),
            ("error", crate::response::ERROR_TEMPLATE),
            ("outcome_unknown", crate::response::OUTCOME_UNKNOWN_TEMPLATE),
        ])
        .unwrap()
    }

    fn decision(vetoes: Vec<Veto>) -> Decision {
        Decision {
            vetoes,
            warnings: vec![Warning::Unmerged],
            fingerprint: Some(
                Fingerprint::parse(
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .unwrap(),
            ),
        }
    }

    #[test]
    fn preview_fixture_with_vetoes() {
        let view = preview_view(
            "0123456789ab/task-1".to_owned(),
            std::path::Path::new("/tmp/w/demo--0123456789ab/task-1"),
            &decision(vec![
                Veto::Dirty,
                Veto::IgnoredNotDisposable {
                    paths: vec!["dist/".to_owned(), "node_modules/".to_owned()],
                },
            ]),
            &[std::path::PathBuf::from("target/")],
            &Some("0f1e2d3c4b5a6978879665544332211ff1e2d3c4".to_owned()),
            &Some("refs/heads/aw/task-1".to_owned()),
        );
        let text = templates()
            .render("remove_worktree", &view, Class::Entity)
            .unwrap();
        assert_eq!(
            text,
            include_str!("../../tests/fixtures/remove_worktree.txt")
        );
    }

    #[test]
    fn receipt_fixture() {
        let view = ReceiptView {
            status: "COMMITTED",
            key: "0123456789ab/task-1".to_owned(),
            path: "/tmp/w/demo--0123456789ab/task-1 (removed)".to_owned(),
            branch: "retained (aw/task-1)".to_owned(),
            warnings: None,
        };
        let text = templates()
            .render("remove_worktree_receipt", &view, Class::Ack)
            .unwrap();
        assert_eq!(
            text,
            include_str!("../../tests/fixtures/remove_worktree_receipt.txt")
        );
    }

    #[tokio::test]
    async fn unknown_field_rejected() {
        let result = call(
            serde_json::json!({"repo":"/repo","mode":"preview","nope":1}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR invalid_arguments:"), "{text}");
    }

    #[tokio::test]
    async fn invalid_mode_refused() {
        let result = call(
            serde_json::json!({"repo":"/repo","name":"task-1","mode":"force"}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR invalid_mode:"), "{text}");
    }

    #[tokio::test]
    async fn apply_without_fingerprint_refused_before_any_state() {
        let result = call(
            serde_json::json!({"repo":"/repo","name":"task-1","mode":"apply"}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR fingerprint_required:"), "{text}");
    }

    #[tokio::test]
    async fn target_required_refused_before_any_state() {
        let result = call(
            serde_json::json!({"repo":"/repo","mode":"preview"}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR target_required:"), "{text}");
    }

    #[test]
    fn degraded_receipt_keeps_the_facts() {
        let text = degraded("COMMITTED", "ab12/task-1", "/w/task-1");
        assert!(text.starts_with("COMMITTED remove_worktree ab12/task-1\n"));
        assert!(text.contains("Do not repeat the removal"));
    }
}
