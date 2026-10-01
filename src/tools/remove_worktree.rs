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
/// Page form used by batch preview mode.
pub const BATCH_TEMPLATE: &str =
    include_str!("../../assets/mcp/tools/remove_worktree_batch.txt.j2");
/// Page form used by batch apply mode.
pub const BATCH_RECEIPT_TEMPLATE: &str =
    include_str!("../../assets/mcp/tools/remove_worktree_batch_receipt.txt.j2");

/// Argument hint used in invalid-arguments refusals.
const ARGS: &str = "Accepted fields: repo, name?|path?, mode (preview|apply), disposable_paths?, allow_unmerged?, fingerprint? (apply), resume_interrupted?; or repo, mode and targets (1-20 items of name?|path?, disposable_paths?, allow_unmerged?, fingerprint?, resume_interrupted?) instead of the single-target fields.";

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

/// One compact line of a batch preview.
#[derive(Serialize)]
struct BatchPreviewRowView {
    key: String,
    path: String,
    head: String,
    branch: String,
    eligible: bool,
    vetoes: String,
    warnings: String,
    fingerprint: String,
}

/// Typed batch preview view rendered by the batch page template.
#[derive(Serialize)]
struct BatchPreviewView {
    count: usize,
    rows: Vec<BatchPreviewRowView>,
    eligible: usize,
    refused: usize,
}

/// One compact line of a batch apply receipt.
#[derive(Serialize)]
struct BatchApplyRowView {
    key: String,
    outcome: String,
}

/// Typed batch apply receipt view rendered by the batch receipt template.
#[derive(Serialize)]
struct BatchReceiptView {
    status: &'static str,
    total: usize,
    removed: usize,
    refused: usize,
    unknown: usize,
    rows: Vec<BatchApplyRowView>,
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

/// Builds one compact batch preview line view from a service row: the
/// single-target facts when the target was assessed, or the stable refusal
/// code when it was not.
fn batch_preview_row(row: &service::BatchPreviewRow) -> BatchPreviewRowView {
    match &row.decision {
        Some(decision) => BatchPreviewRowView {
            key: row.key.clone(),
            path: row.path.clone(),
            head: row.head.clone().unwrap_or_else(|| "unknown".to_owned()),
            branch: row
                .branch
                .as_deref()
                .map(response::short_branch)
                .unwrap_or("detached")
                .to_owned(),
            eligible: decision.vetoes.is_empty(),
            vetoes: if decision.vetoes.is_empty() {
                "-".to_owned()
            } else {
                response::bounded(
                    &decision
                        .vetoes
                        .iter()
                        .map(|veto| {
                            let view = veto_view(veto);
                            format!("{}{}", view.code, view.detail)
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                    240,
                )
            },
            warnings: response::warning_line(&decision.warnings).unwrap_or_else(|| "-".to_owned()),
            fingerprint: decision
                .fingerprint
                .as_ref()
                .map_or_else(|| "-".to_owned(), |f| f.as_str().to_owned()),
        },
        None => BatchPreviewRowView {
            key: row.key.clone(),
            path: row.path.clone(),
            head: "unknown".to_owned(),
            branch: "detached".to_owned(),
            eligible: false,
            vetoes: row
                .refusal
                .clone()
                .unwrap_or_else(|| "probe_unknown".to_owned()),
            warnings: "-".to_owned(),
            fingerprint: "-".to_owned(),
        },
    }
}

/// Outcome text of one batch apply line; the refused codes are bounded to
/// keep the receipt inside the pre-checked page budget.
fn batch_outcome_text(kind: &service::BatchApplyKind) -> String {
    match kind {
        service::BatchApplyKind::Removed => "removed".to_owned(),
        service::BatchApplyKind::AlreadyAbsent => "already_absent".to_owned(),
        service::BatchApplyKind::Refused(codes) => {
            format!("refused {}", response::bounded(codes, 240))
        }
        service::BatchApplyKind::OutcomeUnknown { path } => format!(
            "outcome_unknown {path} (the removal may still be running; inspect before any retry)"
        ),
    }
}

/// Renders one batch preview line exactly as the template lays it out; the
/// single source for the exact page-budget check and the Rust fallback.
fn batch_preview_line(row: &BatchPreviewRowView) -> String {
    format!(
        "{} | {} | head={} | branch={} | eligible={} | vetoes={} | warnings={} | fingerprint={}",
        row.key,
        row.path,
        row.head,
        row.branch,
        row.eligible,
        row.vetoes,
        row.warnings,
        row.fingerprint
    )
}

/// Exact byte size of the batch preview page as it will be rendered.
fn batch_preview_bytes(rows: &[BatchPreviewRowView], eligible: usize, refused: usize) -> usize {
    let mut size = format!("PREVIEW remove_worktree batch: {} target(s)\n", rows.len()).len();
    for row in rows {
        size += batch_preview_line(row).len() + 1;
    }
    size += format!("Summary: eligible={eligible} refused={refused}\n").len();
    size
}

/// Rust-side complete batch preview page used when the template fails; the
/// observation already happened, so every row stays visible.
fn batch_preview_fallback(rows: &[BatchPreviewRowView], eligible: usize, refused: usize) -> String {
    let mut text = format!("PREVIEW remove_worktree batch: {} target(s)\n", rows.len());
    for row in rows {
        text.push_str(&batch_preview_line(row));
        text.push('\n');
    }
    text.push_str(&format!(
        "Summary: eligible={eligible} refused={refused}\n\
Presentation: degraded (presentation_failed).\n"
    ));
    text
}

/// Rust-side batch receipt used when the template fails after a batch ran.
fn batch_degraded(
    rows: &[service::BatchApplyRow],
    removed: usize,
    refused: usize,
    unknown: usize,
) -> String {
    let mut text = format!(
        "COMMITTED remove_worktree batch: {} target(s)\n",
        rows.len()
    );
    for row in rows {
        text.push_str(&format!(
            "{} | {}\n",
            row.key,
            batch_outcome_text(&row.kind)
        ));
    }
    text.push_str(&format!(
        "Summary: removed={removed} refused={refused} unknown={unknown}\n\
Presentation: degraded (presentation_failed).\n\
Do not repeat the removal to repair this response.\n"
    ));
    text
}

pub fn definition() -> Value {
    json!({"name":"remove_worktree",
        "description":"Remove one worktree, or a batch of up to 20 targets of one repository. mode=preview assesses without effect and returns a fingerprint plus vetoes; mode=apply removes under that fingerprint. Never forces and never deletes the branch; dirty, locked, protected or unverified trees are refused. A tree left half-deleted by an interrupted removal (tracked-file deletions only) is resumed when the record shows our own interrupted apply, or when resume_interrupted explicitly says so. An interrupted apply stays visible through the record's removal_started marker.",
        "inputSchema":{"type":"object","required":["repo","mode"],
            "properties":{
                "repo":{"type":"string","description":"Repository root or any worktree inside it; the single repository every target belongs to."},
                "name":{"type":"string","description":"Worktree directory name; provide name or path, not both."},
                "path":{"type":"string","description":"Absolute worktree path; provide name or path, not both."},
                "mode":{"type":"string","enum":["preview","apply"],"description":"preview assesses; apply removes under the preview fingerprint."},
                "disposable_paths":{"type":"array","maxItems":64,"items":{"type":"string","maxLength":256},"description":"Ignored paths approved for deletion, worktree-relative (for example target/)."},
                "allow_unmerged":{"type":"boolean","description":"Explicitly allow removing an unmerged worktree; the branch is retained."},
                "fingerprint":{"type":"string","maxLength":64,"description":"Fingerprint returned by the preview being applied; required for apply."},
                "resume_interrupted":{"type":"boolean","description":"Assert this target's deletions-only tracked changes are an interrupted removal to finish, restoring exactly those files; part of the fingerprint. Automatic when the record has removal_started."},
                "targets":{"type":"array","minItems":1,"maxItems":20,"description":"Batch form: 1-20 targets removed in one call, all in repo. Mutually exclusive with name, path, disposable_paths, allow_unmerged, fingerprint and resume_interrupted.","items":{"type":"object","required":[],"properties":{
                    "name":{"type":"string","maxLength":64,"description":"Worktree directory name; provide exactly one of name or path."},
                    "path":{"type":"string","maxLength":1024,"description":"Absolute worktree path; provide exactly one of name or path."},
                    "disposable_paths":{"type":"array","maxItems":64,"items":{"type":"string","maxLength":256},"description":"Ignored paths approved for this target's deletion, worktree-relative."},
                    "allow_unmerged":{"type":"boolean","description":"Explicitly allow removing this unmerged worktree; the branch is retained."},
                    "fingerprint":{"type":"string","maxLength":64,"description":"Fingerprint returned by this target's preview; required for apply."},
                    "resume_interrupted":{"type":"boolean","description":"Assert this target's deletions-only tracked changes are an interrupted removal to finish; part of the fingerprint."}},
                    "additionalProperties":false}}},
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
        Ok(RemoveOutcome::BatchPreview { rows, eligible }) => {
            let views = rows.iter().map(batch_preview_row).collect::<Vec<_>>();
            let refused = views.len().saturating_sub(eligible);
            // The exact rendered size is a true bound (it is the render): a
            // preview that cannot fit the page budget is refused with a clear
            // code instead of degrading into a row-less fallback. No effect
            // happened, so refusing after the observation is safe.
            if batch_preview_bytes(&views, eligible, refused) > Class::Page.bytes() {
                let error = service::ServiceError::blocked(
                    "batch_too_large",
                    "the batch preview cannot fit the 8 KiB page budget without truncation",
                )
                .with_next("retry with a smaller batch, or one remove_worktree call per target");
                return response::failure(templates, &error);
            }
            let view = BatchPreviewView {
                count: views.len(),
                rows: views,
                eligible,
                refused,
            };
            match templates.render("remove_worktree_batch", &view, Class::Page) {
                Ok(text) => response::text_result(text, false),
                // The observation already happened: the complete Rust page
                // keeps every row visible instead of a generic read fallback.
                Err(_) => response::text_result(
                    batch_preview_fallback(&view.rows, eligible, refused),
                    false,
                ),
            }
        }
        Ok(RemoveOutcome::BatchApplied {
            rows,
            removed,
            refused,
            unknown,
        }) => {
            // A batch reply reports every target's own outcome on its line, so
            // refusals do not make the call an execution error; an unknown
            // effect does, because it demands reconciliation.
            let status = if refused > 0 || unknown > 0 {
                "PARTIAL"
            } else if removed > 0 {
                "COMMITTED"
            } else {
                "NOOP"
            };
            let is_error = unknown > 0;
            let view = BatchReceiptView {
                status,
                total: rows.len(),
                removed,
                refused,
                unknown,
                rows: rows
                    .iter()
                    .map(|row| BatchApplyRowView {
                        key: row.key.clone(),
                        outcome: batch_outcome_text(&row.kind),
                    })
                    .collect(),
            };
            match templates.render("remove_worktree_batch_receipt", &view, Class::Page) {
                Ok(text) => response::text_result(text, is_error),
                Err(_) => response::text_result(
                    batch_degraded(&rows, removed, refused, unknown),
                    is_error,
                ),
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
            ("remove_worktree_batch", BATCH_TEMPLATE),
            ("remove_worktree_batch_receipt", BATCH_RECEIPT_TEMPLATE),
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

    #[tokio::test]
    async fn batch_form_passes_argument_validation() {
        // The batch form itself is well-formed; the failure must come from the
        // nonexistent repository, not from argument validation.
        let result = call(
            serde_json::json!({"repo":"/repo","mode":"preview",
                "targets":[{"name":"task-1"},{"path":"/w/task-2"}]}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        let text = crate::response::first_text(&result);
        for refused in [
            "ERROR invalid_arguments",
            "ERROR targets_conflict",
            "ERROR targets_invalid",
            "ERROR target_required",
            "ERROR fingerprint_required",
        ] {
            assert!(!text.starts_with(refused), "{text}");
        }
        assert!(text.starts_with("ERROR "), "{text}");
    }

    #[tokio::test]
    async fn batch_conflicts_with_single_target_fields() {
        for extra in [
            serde_json::json!({"name":"task-1"}),
            serde_json::json!({"path":"/w/task-1"}),
            serde_json::json!({"disposable_paths":["target"]}),
            serde_json::json!({"allow_unmerged":true}),
            serde_json::json!({"fingerprint":"0".repeat(64)}),
        ] {
            let mut args = serde_json::json!({"repo":"/repo","mode":"preview",
                "targets":[{"name":"task-1"}]});
            let object = args.as_object_mut().unwrap();
            for (key, value) in extra.as_object().unwrap() {
                object.insert(key.clone(), value.clone());
            }
            let result = call(args, &templates(), &Service::new().unwrap()).await;
            let text = crate::response::first_text(&result);
            assert!(text.starts_with("ERROR targets_conflict:"), "{text}");
        }
    }

    #[tokio::test]
    async fn batch_target_count_and_shape_validated() {
        let templates = templates();
        let service = Service::new().unwrap();
        // Empty and over-20 batches refuse.
        for (label, count) in [("empty", 0), ("too many", 21)] {
            let targets: Vec<_> = (0..count)
                .map(|index| serde_json::json!({"name": format!("task-{index}")}))
                .collect();
            let result = call(
                serde_json::json!({"repo":"/repo","mode":"preview","targets":targets}),
                &templates,
                &service,
            )
            .await;
            let text = crate::response::first_text(&result);
            assert!(
                text.starts_with("ERROR targets_invalid:"),
                "{label}: {text}"
            );
        }
        // A target with both or neither of name and path refuses.
        for target in [
            serde_json::json!({"name":"a","path":"/w/a"}),
            serde_json::json!({"disposable_paths":["target"]}),
        ] {
            let result = call(
                serde_json::json!({"repo":"/repo","mode":"preview","targets":[target]}),
                &templates,
                &service,
            )
            .await;
            let text = crate::response::first_text(&result);
            assert!(text.starts_with("ERROR targets_invalid:"), "{text}");
        }
    }

    #[tokio::test]
    async fn batch_apply_requires_every_targets_fingerprint() {
        let result = call(
            serde_json::json!({"repo":"/repo","mode":"apply",
                "targets":[{"name":"task-1","fingerprint":"0".repeat(64)},
                    {"name":"task-2"}]}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR fingerprint_required:"), "{text}");
        assert!(text.contains("target 2"), "{text}");
    }

    #[tokio::test]
    async fn oversized_batch_is_refused_before_any_effect() {
        let long = format!("/w/{}", "d".repeat(600));
        let targets: Vec<_> = (0..20)
            .map(|_| serde_json::json!({"path": &long}))
            .collect();
        let result = call(
            serde_json::json!({"repo":"/repo","mode":"apply","targets":targets}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR batch_too_large:"), "{text}");
        assert!(text.contains("smaller batch"), "{text}");
    }

    #[test]
    fn batch_templates_render_compact_lines() {
        let fingerprint = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let preview = BatchPreviewView {
            count: 2,
            rows: vec![
                BatchPreviewRowView {
                    key: "0123456789ab/task-1".to_owned(),
                    path: "/w/demo--0123456789ab/task-1".to_owned(),
                    head: "0f1e".to_owned(),
                    branch: "aw/task-1".to_owned(),
                    eligible: true,
                    vetoes: "-".to_owned(),
                    warnings: "-".to_owned(),
                    fingerprint: fingerprint.to_owned(),
                },
                BatchPreviewRowView {
                    key: "0123456789ab/task-2".to_owned(),
                    path: "/w/demo--0123456789ab/task-2".to_owned(),
                    head: "0f1e".to_owned(),
                    branch: "aw/task-2".to_owned(),
                    eligible: false,
                    vetoes: "untracked_files".to_owned(),
                    warnings: "-".to_owned(),
                    fingerprint: fingerprint.to_owned(),
                },
            ],
            eligible: 1,
            refused: 1,
        };
        let text = templates()
            .render("remove_worktree_batch", &preview, Class::Page)
            .unwrap();
        assert!(text.starts_with("PREVIEW remove_worktree batch: 2 target(s)\n"));
        assert!(text.contains("/task-1 | /w/demo--0123456789ab/task-1 | head=0f1e"));
        assert!(text.contains("eligible=true | vetoes=- |"));
        assert!(text.contains("eligible=false | vetoes=untracked_files |"));
        assert!(text.ends_with("Summary: eligible=1 refused=1\n"));

        let receipt = BatchReceiptView {
            status: "PARTIAL",
            total: 2,
            removed: 1,
            refused: 1,
            unknown: 0,
            rows: vec![
                BatchApplyRowView {
                    key: "0123456789ab/task-1".to_owned(),
                    outcome: "removed".to_owned(),
                },
                BatchApplyRowView {
                    key: "0123456789ab/task-2".to_owned(),
                    outcome: "refused untracked_files".to_owned(),
                },
            ],
        };
        let text = templates()
            .render("remove_worktree_batch_receipt", &receipt, Class::Page)
            .unwrap();
        assert!(text.starts_with("PARTIAL remove_worktree batch: 2 target(s)\n"));
        assert!(text.contains("/task-1 | removed\n"));
        assert!(text.contains("/task-2 | refused untracked_files\n"));
        assert!(text.ends_with("Summary: removed=1 refused=1 unknown=0\n"));
    }

    #[test]
    fn degraded_batch_receipt_keeps_every_line() {
        let rows = vec![
            service::BatchApplyRow {
                key: "ab12/task-1".to_owned(),
                kind: service::BatchApplyKind::Removed,
            },
            service::BatchApplyRow {
                key: "ab12/task-2".to_owned(),
                kind: service::BatchApplyKind::OutcomeUnknown {
                    path: "/w/task-2".to_owned(),
                },
            },
        ];
        let text = batch_degraded(&rows, 1, 0, 1);
        assert!(text.contains("ab12/task-1 | removed\n"));
        assert!(text.contains("ab12/task-2 | outcome_unknown /w/task-2"));
        assert!(text.contains("Summary: removed=1 refused=0 unknown=1"));
        assert!(text.contains("Do not repeat the removal"));
    }

    #[test]
    fn batch_preview_fallback_keeps_every_row() {
        let fingerprint = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let row = |key: &str, eligible: bool, vetoes: &str| BatchPreviewRowView {
            key: key.to_owned(),
            path: format!("/w/{}", key),
            head: "0f1e".to_owned(),
            branch: format!("aw/{}", key),
            eligible,
            vetoes: vetoes.to_owned(),
            warnings: "-".to_owned(),
            fingerprint: fingerprint.to_owned(),
        };
        let rows = vec![
            row("ab12/task-1", true, "-"),
            row("ab12/task-2", false, "untracked_files"),
        ];
        let text = batch_preview_fallback(&rows, 1, 1);
        assert!(text.starts_with("PREVIEW remove_worktree batch: 2 target(s)\n"));
        assert!(text.contains("/task-1 | /w/ab12/task-1 | head=0f1e"));
        assert!(text.contains("eligible=true | vetoes=-"));
        assert!(text.contains("eligible=false | vetoes=untracked_files"));
        assert!(text.ends_with(
            "Summary: eligible=1 refused=1\nPresentation: degraded (presentation_failed).\n"
        ));
        // The exact byte count of the rendered page matches the checker.
        let degraded_note = "Presentation: degraded (presentation_failed).\n".len();
        assert_eq!(batch_preview_bytes(&rows, 1, 1), text.len() - degraded_note);
    }

    #[test]
    fn degraded_receipt_keeps_the_facts() {
        let text = degraded("COMMITTED", "ab12/task-1", "/w/task-1");
        assert!(text.starts_with("COMMITTED remove_worktree ab12/task-1\n"));
        assert!(text.contains("Do not repeat the removal"));
    }
}
