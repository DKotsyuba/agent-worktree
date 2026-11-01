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

/// Structured activity timestamps; `null` marks a signal that was not
/// measured, never zero or "long ago".
#[derive(Serialize)]
struct SignalsDto {
    /// mtime of the HEAD file, when measured.
    head_mtime: Option<u64>,
    /// mtime of the index file, when measured.
    index_mtime: Option<u64>,
    /// Timestamp inside the latest HEAD reflog entry, when measured.
    last_reflog_entry: Option<u64>,
}

/// Structured hygiene line from data this call already collected.
#[derive(Serialize)]
struct HygieneDto {
    /// Whether the activity band is `idle_candidate` or `stale_candidate`.
    stale_activity: bool,
    /// Whether a measured size reached the policy's large threshold.
    large: bool,
    /// Whether the registered path is absent on disk.
    missing: bool,
}

/// Structured `removal_started` marker.
#[derive(Serialize)]
struct RemovalStartedDto {
    /// Fingerprint accepted at apply time.
    fingerprint: String,
    /// Unix seconds when the removal was dispatched.
    at: u64,
}

/// Structured record projection: attribution, revision and the removal
/// marker. Never raw config or credentials — attribution is not authentication.
#[derive(Serialize)]
struct RecordDto {
    /// Creator harness attribution stored in the record.
    creator: String,
    /// Creation time (unix seconds).
    created_at: u64,
    /// Caller-stated purpose, or `null`.
    purpose: Option<String>,
    /// Creating session identifier, or `null`.
    session: Option<String>,
    /// Optimistic concurrency counter of the record.
    revision: u64,
    /// Marker written before a removal was dispatched, or `null`.
    removal_started: Option<RemovalStartedDto>,
}

/// The five independent probe axes, each carrying its own state.
#[derive(Serialize)]
struct ProbesDto {
    /// Ancestry under the integration ref.
    integration: response::ProbeDto,
    /// Status counts from `git status --porcelain=v2`.
    status: response::ProbeDto,
    /// Submodule dirtiness.
    submodules: response::ProbeDto,
    /// Live processes with cwd inside the tree.
    processes: response::ProbeDto,
    /// On-disk size.
    size: response::ProbeDto,
}

/// Structured detail; the machine-readable twin of the text reply.
///
/// `null` means absent or not measured; a degraded probe keeps its own state
/// instead of collapsing into a value.
#[derive(Serialize)]
struct DetailDto<'a> {
    /// Stable tool key `<id12>/<name>`.
    key: &'a str,
    /// Full 64-hex repository identity.
    repo_id: &'a str,
    /// Canonical absolute Git common directory behind `repo_id`.
    repo_common_dir: String,
    /// Ownership classification label.
    class: &'static str,
    /// Absolute worktree path.
    path: String,
    /// HEAD commit when Git reported it.
    head: Option<&'a str>,
    /// Full branch ref when attached.
    branch: Option<&'a str>,
    /// Whether HEAD is detached.
    detached: bool,
    /// Bounded `git worktree lock` reason, or `null` when unlocked.
    locked: Option<String>,
    /// Activity band; `unknown` is never proof of abandonment.
    activity: &'static str,
    /// Cheap activity timestamps actually measured.
    activity_signals: SignalsDto,
    /// The five independent probe axes.
    probes: ProbesDto,
    /// Advisory warnings as stable codes plus bounded facts.
    warnings: Vec<serde_json::Value>,
    /// Stored record, or `null` when none exists.
    record: Option<RecordDto>,
    /// Hygiene facts from data this call already collected.
    hygiene: HygieneDto,
    /// Unix seconds when the observation batch finished.
    observed_at: u64,
}

/// JSON value of one mergedness state.
fn integration_json(value: &Integration) -> serde_json::Value {
    serde_json::json!(integration_value(*value))
}

/// JSON value of the status facts the contract exposes: the counts, with the
/// ignored entries reduced to their number (paths stay a removal-preview fact).
fn status_json(facts: &StatusFacts) -> serde_json::Value {
    serde_json::json!({
        "staged": facts.staged,
        "unstaged": facts.unstaged,
        "untracked": facts.untracked,
        "conflicts": facts.conflicts,
        "ignored": facts.ignored.len(),
    })
}

/// JSON value of one submodule observation.
fn submodules_json(facts: &SubmoduleFacts) -> serde_json::Value {
    serde_json::json!({"dirty": facts.dirty, "unsupported": facts.unsupported})
}

/// JSON value of one size measurement.
fn size_json(size: &Size) -> serde_json::Value {
    serde_json::json!({
        "bytes": size.bytes,
        "quality": match size.quality {
            SizeQuality::Complete => "complete",
            SizeQuality::LowerBound => "lower_bound",
        },
    })
}

/// Whether a measured size reaches the policy's large threshold; an
/// unmeasured, unavailable or unknown probe never counts as large.
fn large_size(observation: &crate::worktree::Observation) -> bool {
    match &observation.size {
        Probe::Known(size) | Probe::Incomplete { evidence: size, .. } => {
            size.bytes >= crate::worktree::Policy::default().size_warning_bytes
        }
        _ => false,
    }
}

/// Builds the structured detail payload for one inspection outcome.
fn structured_detail(outcome: &InspectOutcome) -> serde_json::Value {
    let observation = &outcome.observation;
    let large = large_size(observation);
    let signals = observation.activity_signals;
    let record = outcome.record.as_ref().map(|record| RecordDto {
        creator: record.creator.clone(),
        created_at: record.created_at,
        purpose: record.purpose.clone(),
        session: record.session.clone(),
        revision: record.revision,
        removal_started: record
            .removal_started
            .as_ref()
            .map(|started| RemovalStartedDto {
                fingerprint: started.fingerprint.as_str().to_owned(),
                at: started.at,
            }),
    });
    serde_json::to_value(DetailDto {
        key: &outcome.key,
        repo_id: observation.id.repo.as_str(),
        repo_common_dir: outcome.common_dir.display().to_string(),
        class: crate::service::class_label(outcome.class),
        path: observation.registration.path.display().to_string(),
        head: observation.registration.head.as_deref(),
        branch: observation.registration.branch.as_deref(),
        detached: observation.registration.detached,
        locked: observation
            .registration
            .locked
            .as_deref()
            .map(|reason| response::bounded(reason, 200)),
        activity: activity(outcome.advice.activity),
        activity_signals: SignalsDto {
            head_mtime: signals.head_mtime,
            index_mtime: signals.index_mtime,
            last_reflog_entry: signals.last_reflog_entry,
        },
        probes: ProbesDto {
            integration: response::ProbeDto::from_probe(&observation.integration, integration_json),
            status: response::ProbeDto::from_probe(&observation.status, status_json),
            submodules: response::ProbeDto::from_probe(&observation.submodules, submodules_json),
            processes: response::ProbeDto::from_probe(&observation.live_processes, |present| {
                serde_json::json!(present)
            }),
            size: response::ProbeDto::from_probe(&observation.size, size_json),
        },
        warnings: outcome
            .advice
            .warnings
            .iter()
            .map(response::warning_value)
            .collect(),
        record,
        hygiene: HygieneDto {
            stale_activity: matches!(
                outcome.advice.activity,
                Activity::IdleCandidate | Activity::StaleCandidate
            ),
            large,
            missing: outcome.class == crate::worktree::WorktreeClass::Missing,
        },
        observed_at: observation.observed_at,
    })
    .unwrap_or(serde_json::Value::Null)
}

/// Assembles the full reply for one successful inspection: the unchanged text
/// reply plus the structured twin. When the text cannot be rendered the reply
/// is the read fallback with `isError=true` and the structured data is still
/// attached — presentation failure never erases a usable read result.
fn detail_reply(templates: &Templates, outcome: &InspectOutcome) -> CallToolResult {
    let structured =
        response::read_envelope("worktree_detail", Some(structured_detail(outcome)), None);
    let observation = &outcome.observation;
    let large = large_size(observation);
    let stale = matches!(
        outcome.advice.activity,
        Activity::IdleCandidate | Activity::StaleCandidate
    );
    let warnings = response::warning_line(&outcome.advice.warnings);
    let view = View {
        key: outcome.key.clone(),
        class: crate::service::class_label(outcome.class),
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
        activity: activity(outcome.advice.activity),
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
        removal_started: outcome
            .record
            .as_ref()
            .and_then(|r| r.removal_started.as_ref())
            .map(|started| {
                format!(
                    "fingerprint {} at {}",
                    started.fingerprint.as_str(),
                    started.at
                )
            }),
        record: outcome.record.as_ref().map(|r| {
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
            outcome.class == crate::worktree::WorktreeClass::Missing
        ),
        observed_at: observation.observed_at,
    };
    match templates.render("inspect_worktree", &view, Class::Entity) {
        Ok(text) => response::with_structured(response::text_result(text, false), structured),
        // A read that cannot be presented is an error, never partial prose.
        Err(_) => response::with_structured(
            response::text_result(response::READ_FALLBACK.to_owned(), true),
            structured,
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
        "outputSchema":response::read_schema("worktree_detail", &json!({
            "type":"object",
            "required":["key","repo_id","repo_common_dir","class","path","head","branch","detached","locked","activity","activity_signals","probes","warnings","record","hygiene","observed_at"],
            "additionalProperties":false,
            "properties":{
                "key":{"type":"string","description":"Stable tool key `<id12>/<name>`."},
                "repo_id":{"type":"string","description":"Full 64-hex repository identity."},
                "repo_common_dir":{"type":"string","description":"Canonical absolute Git common directory behind repo_id."},
                "class":{"type":"string","enum":["main","managed","foreign","missing","orphan_candidate"]},
                "path":{"type":"string"},
                "head":{"type":["string","null"],"description":"HEAD commit when Git reported it."},
                "branch":{"type":["string","null"],"description":"Full branch ref when attached."},
                "detached":{"type":"boolean"},
                "locked":{"type":["string","null"],"description":"Bounded `git worktree lock` reason; null when unlocked."},
                "activity":{"type":"string","enum":["active","recent","idle_candidate","stale_candidate","unknown"],
                    "description":"unknown means no usable signal, never proof of abandonment."},
                "activity_signals":{"$ref":"#/$defs/signals"},
                "probes":{"$ref":"#/$defs/probes"},
                "warnings":{"type":"array","items":{"$ref":"#/$defs/warning"}},
                "record":{"anyOf":[{"$ref":"#/$defs/record"},{"type":"null"}]},
                "hygiene":{"$ref":"#/$defs/hygiene"},
                "observed_at":{"type":"integer","minimum":0,"description":"Unix seconds when the observation batch finished."}},
            "$defs":{
                "signals":{"type":"object","required":["head_mtime","index_mtime","last_reflog_entry"],
                    "additionalProperties":false,
                    "properties":{
                        "head_mtime":{"type":["integer","null"],"description":"Unix seconds; null when not measured."},
                        "index_mtime":{"type":["integer","null"],"description":"Unix seconds; null when not measured."},
                        "last_reflog_entry":{"type":["integer","null"],"description":"Unix seconds; null when not measured."}}},
                "probe":{"type":"object","required":["state","value","code","reason"],
                    "additionalProperties":false,
                    "properties":{
                        "state":{"type":"string","enum":["known","not_checked","unavailable","incomplete"]},
                        "value":{"description":"Typed value when known; the guaranteed partial evidence when incomplete; null otherwise."},
                        "code":{"type":["string","null"],"description":"Stable reason code when unavailable."},
                        "reason":{"type":["string","null"],"description":"Bounded reason when incomplete."}}},
                "probes":{"type":"object","required":["integration","status","submodules","processes","size"],
                    "additionalProperties":false,
                    "properties":{
                        "integration":{"$ref":"#/$defs/probe"},
                        "status":{"$ref":"#/$defs/probe"},
                        "submodules":{"$ref":"#/$defs/probe"},
                        "processes":{"$ref":"#/$defs/probe"},
                        "size":{"$ref":"#/$defs/probe"}}},
                "warning":{"type":"object","required":["code"],"additionalProperties":false,
                    "properties":{
                        "code":{"type":"string"},
                        "bytes":{"type":"integer","minimum":0,"description":"Present for size_at_least."},
                        "reason":{"type":"string","description":"Bounded; present for probe_incomplete."}}},
                "record":{"type":"object",
                    "required":["creator","created_at","purpose","session","revision","removal_started"],
                    "additionalProperties":false,
                    "properties":{
                        "creator":{"type":"string"},
                        "created_at":{"type":"integer","minimum":0,"description":"Unix seconds."},
                        "purpose":{"type":["string","null"]},
                        "session":{"type":["string","null"]},
                        "revision":{"type":"integer","minimum":0,"description":"Record revision (optimistic concurrency counter)."},
                        "removal_started":{"anyOf":[
                            {"type":"object","required":["fingerprint","at"],"additionalProperties":false,
                             "properties":{"fingerprint":{"type":"string"},"at":{"type":"integer","minimum":0}}},
                            {"type":"null"}]}}},
                "hygiene":{"type":"object","required":["stale_activity","large","missing"],
                    "additionalProperties":false,
                    "properties":{
                        "stale_activity":{"type":"boolean"},
                        "large":{"type":"boolean"},
                        "missing":{"type":"boolean"}}}}
        })),
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}})
}

pub async fn call(args: Value, templates: &Templates, service: &Service) -> CallToolResult {
    let Ok(parsed) = serde_json::from_value::<service::InspectArgs>(args) else {
        return response::read_invalid_arguments(
            templates,
            "inspect_worktree",
            ARGS,
            "worktree_detail",
        );
    };
    match service.inspect_worktree(&parsed).await {
        Ok(outcome) => detail_reply(templates, &outcome),
        // Refusals and unavailability keep their text form and gain the typed
        // structured error outcome under the same output schema.
        Err(error) => response::read_failure(templates, "worktree_detail", &error),
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
                worktree_deletions_only: false,
                deleted_paths: Vec::new(),
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
        // The refused read carries its typed error outcome, never an empty
        // success.
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["schema_version"], 1);
        assert_eq!(structured["kind"], "worktree_detail");
        assert!(structured.get("data").is_none());
        assert_eq!(structured["error"]["code"], "not_a_repository");
        assert_eq!(structured["error"]["outcome"], "blocked");
    }

    /// One inspection outcome covering every probe state at once: status
    /// incomplete, submodules unavailable, processes known, size not checked,
    /// integration known — plus a record with its revision and marker.
    fn sample_outcome() -> InspectOutcome {
        let mut observation = sample_observation();
        observation.status = Probe::Incomplete {
            evidence: StatusFacts {
                staged: 1,
                unstaged: 0,
                untracked: 2,
                conflicts: 0,
                ignored: vec!["target/".to_owned()],
                digest: "cafe01".to_owned(),
                worktree_deletions_only: false,
                deleted_paths: Vec::new(),
            },
            reason: "page budget ran out".to_owned(),
        };
        observation.submodules = Probe::Unavailable {
            code: "submodule_status_failed".to_owned(),
        };
        InspectOutcome {
            key: "0123456789ab/task-1".to_owned(),
            class: crate::worktree::WorktreeClass::Managed,
            common_dir: std::path::PathBuf::from("/repo/.git"),
            observation,
            advice: crate::worktree::Advice {
                activity: crate::worktree::Activity::Recent,
                warnings: vec![crate::worktree::Warning::ProbeIncomplete {
                    reason: "page budget ran out".to_owned(),
                }],
            },
            record: Some(crate::worktree::Record {
                schema_version: 1,
                repo_id: "0".repeat(64),
                name: "task-1".to_owned(),
                path: std::path::PathBuf::from("/tmp/w/demo--0123456789ab/task-1"),
                branch: Some("aw/task-1".to_owned()),
                base_ref: None,
                base_oid: None,
                created_at: 1_799_999_999,
                creator: "claude-code".to_owned(),
                session: Some("sess-7".to_owned()),
                purpose: Some("ship the release".to_owned()),
                revision: 3,
                removal_started: Some(crate::worktree::RemovalStarted {
                    fingerprint: crate::worktree::Fingerprint::parse(&"a".repeat(64)).unwrap(),
                    at: 1_800_000_000,
                }),
            }),
        }
    }

    #[test]
    fn probe_states_stay_independent() {
        let structured = result_detail(&sample_outcome());
        let probes = &structured["data"]["probes"];
        // Known: typed value, no failure fields.
        assert_eq!(probes["integration"]["state"], "known");
        assert_eq!(probes["integration"]["value"], "unmerged");
        assert!(probes["integration"]["code"].is_null());
        assert!(probes["integration"]["reason"].is_null());
        // Incomplete: guaranteed partial evidence plus bounded reason.
        assert_eq!(probes["status"]["state"], "incomplete");
        assert_eq!(probes["status"]["value"]["staged"], 1);
        assert_eq!(probes["status"]["value"]["untracked"], 2);
        assert_eq!(probes["status"]["value"]["ignored"], 1);
        assert!(probes["status"]["code"].is_null());
        assert_eq!(probes["status"]["reason"], "page budget ran out");
        // Unavailable: stable code, no value.
        assert_eq!(probes["submodules"]["state"], "unavailable");
        assert_eq!(probes["submodules"]["code"], "submodule_status_failed");
        assert!(probes["submodules"]["value"].is_null());
        assert!(probes["submodules"]["reason"].is_null());
        // Known false is a real answer, not a missing one.
        assert_eq!(probes["processes"]["state"], "known");
        assert_eq!(probes["processes"]["value"], false);
        // Not checked stays explicitly not checked.
        assert_eq!(probes["size"]["state"], "not_checked");
        assert!(probes["size"]["value"].is_null());
        assert!(probes["size"]["code"].is_null());
        assert!(probes["size"]["reason"].is_null());
    }

    #[test]
    fn structured_detail_carries_identity_record_and_hygiene() {
        let structured = result_detail(&sample_outcome());
        let data = &structured["data"];
        assert_eq!(data["key"], "0123456789ab/task-1");
        let repo_id = crate::worktree::RepoId::from_common_dir(std::path::Path::new("/repo/.git"));
        assert_eq!(data["repo_id"], repo_id.as_str());
        assert_eq!(data["repo_common_dir"], "/repo/.git");
        assert_eq!(data["class"], "managed");
        assert_eq!(data["branch"], "refs/heads/aw/task-1");
        assert_eq!(data["detached"], false);
        assert!(data["locked"].is_null());
        assert_eq!(data["activity"], "recent");
        assert!(data["activity_signals"]["head_mtime"].is_null());
        assert!(data["activity_signals"]["index_mtime"].is_null());
        assert!(data["activity_signals"]["last_reflog_entry"].is_null());
        // Advisory warnings as stable codes.
        assert_eq!(data["warnings"][0]["code"], "probe_incomplete");
        // Record revision and removal marker.
        assert_eq!(data["record"]["revision"], 3);
        assert_eq!(data["record"]["creator"], "claude-code");
        assert_eq!(data["record"]["purpose"], "ship the release");
        assert_eq!(data["record"]["session"], "sess-7");
        assert_eq!(data["record"]["removal_started"]["at"], 1_800_000_000);
        assert_eq!(
            data["record"]["removal_started"]["fingerprint"],
            "a".repeat(64)
        );
        assert_eq!(data["hygiene"]["stale_activity"], false);
        assert_eq!(data["hygiene"]["large"], false);
        assert_eq!(data["hygiene"]["missing"], false);
        assert_eq!(data["observed_at"], 1_800_000_000);
        assert_eq!(structured["kind"], "worktree_detail");
        assert_eq!(structured["schema_version"], 1);
        assert!(structured.get("error").is_none());
    }

    #[test]
    fn render_failure_keeps_structured_data() {
        // A template set without the entity template fails rendering; the
        // reply must stay an error and still carry the structured detail.
        let empty = Templates::new(&[]).unwrap();
        let reply = detail_reply(&empty, &sample_outcome());
        assert_eq!(reply.is_error, Some(true));
        assert_eq!(
            crate::response::first_text(&reply),
            crate::response::READ_FALLBACK
        );
        let structured = reply.structured_content.unwrap();
        assert_eq!(structured["kind"], "worktree_detail");
        assert_eq!(structured["data"]["key"], "0123456789ab/task-1");
        assert_eq!(structured["data"]["probes"]["size"]["state"], "not_checked");
        assert!(structured.get("error").is_none());
    }

    /// Builds the structured envelope for one outcome through the reply path.
    fn result_detail(outcome: &InspectOutcome) -> serde_json::Value {
        detail_reply(&templates(), outcome)
            .structured_content
            .unwrap()
    }
}
