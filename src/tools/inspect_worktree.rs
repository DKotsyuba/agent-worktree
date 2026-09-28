//! `inspect_worktree`: bounded evidence about one worktree.
//!
//! Read-only. Probes that were not requested or that failed are shown as such;
//! an unknown check is never mistaken for a clean result. The reply is partial
//! (still a successful read) when probes degrade, and the hygiene line uses
//! only data this call already collected.
use crate::response::{self, Class, Templates};
use crate::service::{self, InspectOutcome, Service};
use crate::worktree::{
    Activity, Integration, Probe, Size, SizeQuality, StatusFacts, SubmoduleFacts,
};
use rmcp::model::CallToolResult;
use serde::Serialize;
use serde_json::{Value, json};

pub const IMPLEMENTED: bool = true;
pub const TEMPLATE: &str = include_str!("../../assets/mcp/tools/inspect_worktree.txt.j2");

/// Argument hint used in invalid-arguments refusals.
const ARGS: &str = "Accepted fields: repo, name?|path?, checks? \
{status?, integration?, processes?, submodules?, size?}.";

/// Typed entity view rendered by the embedded template.
#[derive(Serialize)]
struct View {
    key: String,
    class: &'static str,
    path: String,
    head: String,
    branch: String,
    activity: &'static str,
    integration: String,
    status: String,
    submodules: String,
    processes: String,
    size: String,
    locked: String,
    record: Option<String>,
    removal_started: Option<String>,
    warnings: Option<String>,
    hygiene: String,
    observed_at: u64,
}

fn activity(activity: Activity) -> &'static str {
    crate::service::activity_label(activity)
}

/// Label for one ancestry value.
fn integration_value(value: Integration) -> &'static str {
    match value {
        Integration::AncestorMerged => "ancestor_merged",
        Integration::Unmerged => "unmerged",
        Integration::Unknown => "unknown",
    }
}

/// Label for the integration probe, preserving degraded states.
fn integration_label(probe: &Probe<Integration>) -> String {
    match probe {
        Probe::Known(value) => integration_value(*value).to_owned(),
        Probe::NotChecked => "not checked".to_owned(),
        Probe::Unavailable { code } => format!("unavailable ({})", response::bounded(code, 60)),
        Probe::Incomplete { evidence, reason } => format!(
            "{} (incomplete: {})",
            integration_value(*evidence),
            response::bounded(reason, 80)
        ),
    }
}

/// Label for the status probe with per-counter facts.
fn status_label(probe: &Probe<StatusFacts>) -> String {
    match probe {
        Probe::Known(facts) => format!(
            "staged={} unstaged={} untracked={} conflicts={} ignored={}",
            facts.staged,
            facts.unstaged,
            facts.untracked,
            facts.conflicts,
            facts.ignored.len()
        ),
        Probe::NotChecked => "not checked".to_owned(),
        Probe::Unavailable { code } => format!("unavailable ({})", response::bounded(code, 60)),
        Probe::Incomplete { evidence, reason } => format!(
            "staged={} unstaged={} untracked={} conflicts={} ignored={} (incomplete: {})",
            evidence.staged,
            evidence.unstaged,
            evidence.untracked,
            evidence.conflicts,
            evidence.ignored.len(),
            response::bounded(reason, 80)
        ),
    }
}

/// Label for the submodule probe.
fn submodules_label(probe: &Probe<SubmoduleFacts>) -> String {
    match probe {
        Probe::Known(facts) => format!("dirty={} unsupported={}", facts.dirty, facts.unsupported),
        Probe::NotChecked => "not checked".to_owned(),
        Probe::Unavailable { code } => format!("unavailable ({})", response::bounded(code, 60)),
        Probe::Incomplete { evidence, reason } => format!(
            "dirty={} unsupported={} (incomplete: {})",
            evidence.dirty,
            evidence.unsupported,
            response::bounded(reason, 80)
        ),
    }
}

/// Label for the live-process probe.
fn processes_label(probe: &Probe<bool>) -> String {
    match probe {
        Probe::Known(true) => "present".to_owned(),
        Probe::Known(false) => "none".to_owned(),
        Probe::NotChecked => "not checked".to_owned(),
        Probe::Unavailable { code } => format!("unavailable ({})", response::bounded(code, 60)),
        Probe::Incomplete { evidence, reason } => format!(
            "{} (incomplete: {})",
            if *evidence { "present" } else { "none" },
            response::bounded(reason, 80)
        ),
    }
}

/// Label for the size probe.
fn size_label(probe: &Probe<Size>) -> String {
    match probe {
        Probe::Known(size) => format!(
            "{}{}",
            response::human_bytes(size.bytes),
            match size.quality {
                SizeQuality::Complete => "",
                SizeQuality::LowerBound => " (lower bound)",
            }
        ),
        Probe::NotChecked => "not checked".to_owned(),
        Probe::Unavailable { code } => format!("unavailable ({})", response::bounded(code, 60)),
        Probe::Incomplete { evidence, reason } => format!(
            "{} (incomplete: {})",
            response::human_bytes(evidence.bytes),
            response::bounded(reason, 80)
        ),
    }
}

pub fn definition() -> Value {
    json!({"name":"inspect_worktree",
        "description":"Inspect one worktree with bounded probes: activity, integration ancestry, status counts, submodules, live processes and optional size. Read-only. Unknown, unavailable or incomplete checks are reported as such, never as clean.",
        "inputSchema":{"type":"object","required":["repo"],
            "properties":{
                "repo":{"type":"string","description":"Repository root or any worktree inside it."},
                "name":{"type":"string","description":"Worktree directory name; provide name or path, not both."},
                "path":{"type":"string","description":"Absolute worktree path; provide name or path, not both."},
                "checks":{"type":"object","description":"Expensive probes to run; when omitted, status, integration and processes run.",
                    "properties":{
                        "status":{"type":"boolean"},
                        "integration":{"type":"boolean"},
                        "processes":{"type":"boolean"},
                        "submodules":{"type":"boolean"},
                        "size":{"type":"boolean"}},
                    "additionalProperties":false}},
            "additionalProperties":false},
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}})
}

pub async fn call(args: Value, templates: &Templates, service: &Service) -> CallToolResult {
    let Ok(parsed) = serde_json::from_value::<service::InspectArgs>(args) else {
        return response::invalid_arguments(templates, "inspect_worktree", ARGS);
    };
    let InspectOutcome {
        key,
        class,
        observation,
        advice,
        record,
    } = match service.inspect_worktree(&parsed).await {
        Ok(outcome) => outcome,
        Err(error) => return response::failure(templates, &error),
    };
    let large = match &observation.size {
        Probe::Known(size) | Probe::Incomplete { evidence: size, .. } => {
            size.bytes >= crate::worktree::Policy::default().size_warning_bytes
        }
        _ => false,
    };
    let stale = matches!(
        advice.activity,
        Activity::IdleCandidate | Activity::StaleCandidate
    );
    let warnings = response::warning_line(&advice.warnings);
    let view = View {
        key,
        class: crate::service::class_label(class),
        path: observation.registration.path.display().to_string(),
        head: observation
            .registration
            .head
            .clone()
            .unwrap_or_else(|| "unknown".to_owned()),
        branch: observation
            .registration
            .branch
            .as_deref()
            .map(response::short_branch)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if observation.registration.detached {
                    "detached".to_owned()
                } else {
                    "unknown".to_owned()
                }
            }),
        activity: activity(advice.activity),
        integration: integration_label(&observation.integration),
        status: status_label(&observation.status),
        submodules: submodules_label(&observation.submodules),
        processes: processes_label(&observation.live_processes),
        size: size_label(&observation.size),
        locked: observation
            .registration
            .locked
            .as_deref()
            .map(|reason| response::bounded(reason, 120))
            .unwrap_or_else(|| "-".to_owned()),
        removal_started: record
            .as_ref()
            .and_then(|r| r.removal_started.as_ref())
            .map(|started| {
                format!(
                    "fingerprint {} at {}",
                    started.fingerprint.as_str(),
                    started.at
                )
            }),
        record: record.as_ref().map(|r| {
            let meta = format!(
                "creator={} created={} purpose={}",
                r.creator,
                response::iso_utc(r.created_at),
                r.purpose.as_deref().unwrap_or("-")
            );
            let extra = match &r.session {
                Some(session) => format!(" session={session}"),
                None => String::new(),
            };
            response::bounded(&format!("{meta}{extra}"), 400)
        }),
        warnings,
        hygiene: format!(
            "stale_activity={} large={} missing={}",
            stale,
            large,
            class == crate::worktree::WorktreeClass::Missing
        ),
        observed_at: observation.observed_at,
    };
    match templates.render("inspect_worktree", &view, Class::Entity) {
        Ok(text) => response::text_result(text, false),
        // A read that cannot be presented is an error, never partial prose.
        Err(_) => response::text_result(response::READ_FALLBACK.to_owned(), true),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Test assertions")]
mod tests {
    use super::*;
    use crate::response::Templates;
    use crate::worktree::{Observation, Registration, WorktreeId};

    fn templates() -> Templates {
        Templates::new(&[
            ("inspect_worktree", TEMPLATE),
            ("error", crate::response::ERROR_TEMPLATE),
            ("outcome_unknown", crate::response::OUTCOME_UNKNOWN_TEMPLATE),
        ])
        .unwrap()
    }

    fn sample_observation() -> Observation {
        Observation {
            id: WorktreeId {
                repo: crate::worktree::RepoId::from_common_dir(std::path::Path::new("/repo/.git")),
                name: "task-1".to_owned(),
            },
            observed_at: 1_800_000_000,
            registration: Registration {
                path: std::path::PathBuf::from("/tmp/w/demo--0123456789ab/task-1"),
                head: Some("0f1e2d3c4b5a6978879665544332211ff1e2d3c4".to_owned()),
                branch: Some("refs/heads/aw/task-1".to_owned()),
                detached: false,
                bare: false,
                locked: None,
                prunable: None,
                is_main: false,
            },
            activity_signals: crate::worktree::ActivitySignals::default(),
            live_processes: Probe::Known(false),
            status: Probe::Known(StatusFacts {
                staged: 0,
                unstaged: 0,
                untracked: 2,
                conflicts: 0,
                ignored: vec!["target/".to_owned()],
                digest: "cafe01".to_owned(),
            }),
            submodules: Probe::NotChecked,
            integration: Probe::Known(Integration::Unmerged),
            size: Probe::NotChecked,
        }
    }

    #[test]
    fn probe_labels_preserve_unknown_states() {
        assert_eq!(integration_label(&Probe::NotChecked), "not checked");
        assert_eq!(
            integration_label(&Probe::Unavailable {
                code: "lsof_failed".to_owned()
            }),
            "unavailable (lsof_failed)"
        );
        assert_eq!(processes_label(&Probe::Known(true)), "present");
        assert!(status_label(&Probe::NotChecked) == "not checked");
    }

    #[test]
    fn template_fixture() {
        let observation = sample_observation();
        let view = View {
            key: "0123456789ab/task-1".to_owned(),
            class: "managed",
            path: "/tmp/w/demo--0123456789ab/task-1".to_owned(),
            head: "0f1e2d3c4b5a6978879665544332211ff1e2d3c4".to_owned(),
            branch: "aw/task-1".to_owned(),
            activity: "stale_candidate",
            integration: "unmerged".to_owned(),
            status: "staged=0 unstaged=0 untracked=2 conflicts=0 ignored=1".to_owned(),
            submodules: "not checked".to_owned(),
            processes: "none".to_owned(),
            size: "not checked".to_owned(),
            locked: "-".to_owned(),
            record: Some(
                "creator=harness created=2027-01-15T08:00:00Z purpose=ship the release".to_owned(),
            ),
            removal_started: None,
            warnings: Some("unmerged".to_owned()),
            hygiene: "stale_activity=true large=false missing=false".to_owned(),
            observed_at: observation.observed_at,
        };
        let text = templates()
            .render("inspect_worktree", &view, Class::Entity)
            .unwrap();
        assert_eq!(
            text,
            include_str!("../../tests/fixtures/inspect_worktree.txt")
        );
    }

    #[tokio::test]
    async fn unknown_field_rejected() {
        let result = call(
            serde_json::json!({"repo":"/repo","name":"task-1","nope":true}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR invalid_arguments:"), "{text}");
    }

    #[tokio::test]
    async fn missing_repo_refused_by_git() {
        let result = call(
            serde_json::json!({"repo":"/repo"}),
            &templates(),
            &Service::new().unwrap(),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        let text = crate::response::first_text(&result);
        assert!(text.starts_with("ERROR not_a_repository:"), "{text}");
    }
}
