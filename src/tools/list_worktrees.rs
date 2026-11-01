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

/// Structured activity band of one page row: the shared activity vocabulary
/// plus the measurement basis, so a page band is never mistaken for the
/// process-verified activity an inspection reports.
#[derive(Serialize)]
struct ActivityDto {
    /// Band from the page pass.
    state: &'static str,
    /// True when the band was derived from HEAD/index mtimes and the last
    /// reflog entry; false for `unknown`, where no signal produced an answer.
    mtime_based: bool,
}

/// Structured numeric size; never a human-formatted unit label.
#[derive(Serialize)]
struct SizeDto {
    /// Measured or lower-bound byte count.
    bytes: u64,
    /// `complete` or `lower_bound` (budget-cut traversal).
    quality: &'static str,
}

/// Structured inventory row; the machine-readable twin of one text row.
///
/// `null` always means the fact is absent or was not probed; it never means
/// zero or clean. `branch`/`head` carry the full authoritative forms.
#[derive(Serialize)]
struct RowDto<'a> {
    /// Stable tool key `<id12>/<name>`.
    key: &'a str,
    /// Full 64-hex repository identity.
    repo_id: &'a str,
    /// Canonical absolute Git common directory behind `repo_id`.
    repo_common_dir: String,
    /// Absolute worktree path.
    path: String,
    /// Ownership classification label.
    class: &'static str,
    /// Full branch ref when attached.
    branch: Option<&'a str>,
    /// Whether HEAD is detached.
    detached: bool,
    /// HEAD commit when Git reported it.
    head: Option<&'a str>,
    /// Record creator when a record backs the row.
    creator: Option<&'a str>,
    /// Whether the record's removal was dispatched but never completed.
    removal_started: bool,
    /// Page activity band, or `null` when the row was not probed.
    activity: Option<ActivityDto>,
    /// Mergedness, or `null` when the row was not probed.
    integration: Option<&'static str>,
    /// Measured size, or `null` when not requested or not measurable.
    size: Option<SizeDto>,
}

/// Structured hygiene counts over exactly the rows this page collected.
#[derive(Serialize)]
struct HygieneDto {
    /// Page rows whose registered path is absent.
    missing: usize,
    /// Page rows that are directories without a registration.
    orphan: usize,
    /// Page rows whose removal was dispatched but never completed.
    removal_started: usize,
    /// Idle rows (mtime-based); includes `stale`.
    idle: usize,
    /// Stale rows (a subset of `idle`).
    stale: usize,
    /// Rows whose HEAD is not merged into the integration ref.
    unmerged: usize,
    /// Whether a measured page row reached the large threshold; only
    /// meaningful when size was requested.
    large: bool,
}

/// One failed repository in the structured coverage.
#[derive(Serialize)]
struct CoverageFailureDto<'a> {
    /// Repository id12 of the scope that failed.
    repo: &'a str,
    /// Stable failure code.
    code: &'a str,
}

/// Structured scope coverage of one list call.
#[derive(Serialize)]
struct CoverageDto<'a> {
    /// Repositories in the resolved scope.
    repos: usize,
    /// Repositories that came from the registry.
    registry: usize,
    /// Repositories found by scanning configured roots.
    discovered: usize,
    /// Repositories whose inventory failed, with stable codes.
    failed: Vec<CoverageFailureDto<'a>>,
    /// Whether the discovery or call budget ran out before covering the scope.
    budget_exhausted: bool,
    /// Whether the per-repo orphan scan hit its entry cap without finishing.
    orphan_scan_truncated: bool,
    /// Whether no worktree root is configured, so the orphan scan was skipped.
    root_unconfigured: bool,
}

/// Structured page data; the machine-readable twin of the text page.
#[derive(Serialize)]
struct PageDto<'a> {
    /// Rows shown on this page.
    returned: usize,
    /// Whether more rows exist in scope after this page.
    has_more: bool,
    /// Cursor resuming after the last shown row; `null` when none remain.
    cursor: Option<&'a str>,
    /// Rows in keyset order.
    rows: Vec<RowDto<'a>>,
    /// Hygiene counts over exactly this page's rows.
    hygiene: HygieneDto,
    /// Scope coverage facts.
    coverage: CoverageDto<'a>,
}

/// Structured value label for one mergedness state.
fn integration_value(value: Integration) -> &'static str {
    match value {
        Integration::AncestorMerged => "ancestor_merged",
        Integration::Unmerged => "unmerged",
        Integration::Unknown => "unknown",
    }
}

/// Builds one structured row; values stay numeric or stable codes, never
/// display labels, and absent facts stay `null`.
fn structured_row(row: &ListRow) -> RowDto<'_> {
    RowDto {
        key: &row.key,
        repo_id: &row.repo_id,
        repo_common_dir: row.common_dir.display().to_string(),
        path: row.path.display().to_string(),
        class: row.class_label(),
        branch: row.branch.as_deref(),
        detached: row.detached,
        head: row.head.as_deref(),
        creator: row.creator.as_deref(),
        removal_started: row.removal_started,
        activity: row.activity.map(|band| ActivityDto {
            state: crate::service::activity_label(band),
            mtime_based: band != Activity::Unknown,
        }),
        integration: row.integration.map(integration_value),
        size: row.size.map(|size| SizeDto {
            bytes: size.bytes,
            quality: match size.quality {
                crate::worktree::SizeQuality::Complete => "complete",
                crate::worktree::SizeQuality::LowerBound => "lower_bound",
            },
        }),
    }
}

/// Builds the structured page payload for one list outcome.
fn structured_page(outcome: &ListOutcome) -> serde_json::Value {
    serde_json::to_value(PageDto {
        returned: outcome.rows.len(),
        has_more: outcome.has_more,
        cursor: outcome.cursor.as_deref(),
        rows: outcome.rows.iter().map(structured_row).collect(),
        hygiene: HygieneDto {
            missing: outcome.hygiene.missing,
            orphan: outcome.hygiene.orphan,
            removal_started: outcome.hygiene.removal_started,
            idle: outcome.hygiene.idle,
            stale: outcome.hygiene.stale,
            unmerged: outcome.hygiene.unmerged,
            large: outcome.hygiene.large,
        },
        coverage: CoverageDto {
            repos: outcome.coverage.repos,
            registry: outcome.coverage.registry,
            discovered: outcome.coverage.discovered,
            failed: outcome
                .coverage
                .failed
                .iter()
                .map(|(repo, code)| CoverageFailureDto { repo, code })
                .collect(),
            budget_exhausted: outcome.coverage.budget_exhausted,
            orphan_scan_truncated: outcome.coverage.orphan_scan_truncated,
            root_unconfigured: outcome.coverage.root_unconfigured,
        },
    })
    .unwrap_or(serde_json::Value::Null)
}

/// Assembles the full reply for one successful list outcome: the unchanged
/// text page plus the structured twin. When the text cannot be rendered the
/// reply is the read fallback with `isError=true` and the structured data is
/// still attached — presentation failure never erases a usable read result.
fn page_reply(templates: &Templates, outcome: &ListOutcome, include_size: bool) -> CallToolResult {
    let structured = response::read_envelope("worktree_page", Some(structured_page(outcome)), None);
    let status = if outcome.coverage.failed.is_empty() {
        "OK"
    } else {
        "PARTIAL"
    };
    let view = View {
        status,
        returned: outcome.rows.len(),
        has_more: outcome.has_more,
        rows: outcome
            .rows
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
        hygiene: hygiene_label(&outcome.hygiene, include_size),
        coverage: coverage_label(&outcome.coverage),
        cursor: outcome.cursor.clone(),
    };
    match templates.render("list_worktrees", &view, Class::Page) {
        Ok(text) => response::with_structured(response::text_result(text, false), structured),
        // A read whose information could not be presented is an error, never a
        // silently shortened page.
        Err(_) => response::with_structured(
            response::text_result(response::READ_FALLBACK.to_owned(), true),
            structured,
        ),
    }
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
    if coverage.root_unconfigured {
        text.push_str("; worktree root not configured; orphan scan skipped");
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
        "description":"List worktrees across the managed scope with ownership classification (main, managed, foreign, missing, orphan_candidate) and cheap per-row signals: mtime-based activity band (recent~/idle~/stale~) and mergedness. Read-only. Keyset-paginated, at most 20 rows per page; main checkouts are context rows excluded from the hygiene counters, and orphan candidates are never automatically deletable.",
        "inputSchema":{"type":"object",
            "properties":{
                "repo":{"type":"string","description":"Restrict the scope to one repository; omit for all known repositories."},
                "discovery":{"type":"boolean","description":"Also scan configured discovery roots; default true."},
                "cursor":{"type":"string","description":"Cursor from the previous page."},
                "limit":{"type":"integer","minimum":1,"maximum":20,"description":"Rows per page; default 20."},
                "size":{"type":"boolean","description":"Measure on-disk size of the rows shown on this page; default false."}},
            "additionalProperties":false},
        "outputSchema":response::read_schema("worktree_page", &json!({
            "type":"object",
            "required":["returned","has_more","cursor","rows","hygiene","coverage"],
            "additionalProperties":false,
            "properties":{
                "returned":{"type":"integer","minimum":0},
                "has_more":{"type":"boolean"},
                "cursor":{"type":["string","null"],"description":"Cursor resuming after the last row; null when no more rows remain."},
                "rows":{"type":"array","maxItems":20,"items":{"$ref":"#/$defs/row"}},
                "hygiene":{"$ref":"#/$defs/hygiene"},
                "coverage":{"$ref":"#/$defs/coverage"}},
            "$defs":{
                "row":{"type":"object",
                    "required":["key","repo_id","repo_common_dir","path","class","branch","detached","head","creator","removal_started","activity","integration","size"],
                    "additionalProperties":false,
                    "properties":{
                        "key":{"type":"string","description":"Stable tool key `<id12>/<name>`."},
                        "repo_id":{"type":"string","description":"Full 64-hex repository identity."},
                        "repo_common_dir":{"type":"string","description":"Canonical absolute Git common directory behind repo_id."},
                        "path":{"type":"string"},
                        "class":{"type":"string","enum":["main","managed","foreign","missing","orphan_candidate"]},
                        "branch":{"type":["string","null"],"description":"Full branch ref when attached; null when detached or unknown."},
                        "detached":{"type":"boolean"},
                        "head":{"type":["string","null"],"description":"HEAD commit when Git reported it."},
                        "creator":{"type":["string","null"],"description":"Record creator when a record backs the row."},
                        "removal_started":{"type":"boolean"},
                        "activity":{"anyOf":[{"$ref":"#/$defs/activity"},{"type":"null"}],
                            "description":"null when the row was not probed (missing, orphan)."},
                        "integration":{"type":["string","null"],"enum":["ancestor_merged","unmerged","unknown",null],
                            "description":"null when the row was not probed."},
                        "size":{"anyOf":[{"$ref":"#/$defs/size"},{"type":"null"}],
                            "description":"null when not requested or not measurable."}}},
                "activity":{"type":"object","required":["state","mtime_based"],"additionalProperties":false,
                    "properties":{
                        "state":{"type":"string","enum":["active","recent","idle_candidate","stale_candidate","unknown"]},
                        "mtime_based":{"type":"boolean","description":"true: band from HEAD/index mtimes and the last reflog entry, never process-verified."}}},
                "size":{"type":"object","required":["bytes","quality"],"additionalProperties":false,
                    "properties":{
                        "bytes":{"type":"integer","minimum":0},
                        "quality":{"type":"string","enum":["complete","lower_bound"]}}},
                "hygiene":{"type":"object",
                    "required":["missing","orphan","removal_started","idle","stale","unmerged","large"],
                    "additionalProperties":false,
                    "properties":{
                        "missing":{"type":"integer","minimum":0},
                        "orphan":{"type":"integer","minimum":0},
                        "removal_started":{"type":"integer","minimum":0},
                        "idle":{"type":"integer","minimum":0,"description":"Includes stale rows."},
                        "stale":{"type":"integer","minimum":0},
                        "unmerged":{"type":"integer","minimum":0},
                        "large":{"type":"boolean","description":"Whether a measured page row reached the large threshold; only meaningful when size was requested."}}},
                "coverage":{"type":"object",
                    "required":["repos","registry","discovered","failed","budget_exhausted","orphan_scan_truncated","root_unconfigured"],
                    "additionalProperties":false,
                    "properties":{
                        "repos":{"type":"integer","minimum":0},
                        "registry":{"type":"integer","minimum":0},
                        "discovered":{"type":"integer","minimum":0},
                        "failed":{"type":"array","items":{"type":"object","required":["repo","code"],"additionalProperties":false,
                            "properties":{"repo":{"type":"string"},"code":{"type":"string"}}}},
                        "budget_exhausted":{"type":"boolean"},
                        "orphan_scan_truncated":{"type":"boolean"},
                        "root_unconfigured":{"type":"boolean"}}}}
        })),
        "annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}})
}

pub async fn call(args: Value, templates: &Templates, service: &Service) -> CallToolResult {
    let Ok(parsed) = serde_json::from_value::<service::ListArgs>(args) else {
        return response::read_invalid_arguments(
            templates,
            "list_worktrees",
            ARGS,
            "worktree_page",
        );
    };
    let include_size = parsed.size.unwrap_or(false);
    match service.list_worktrees(&parsed).await {
        Ok(outcome) => page_reply(templates, &outcome, include_size),
        // Refusals and unavailability keep their text form and gain the typed
        // structured error outcome under the same output schema.
        Err(error) => response::read_failure(templates, "worktree_page", &error),
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
            common_dir: std::path::PathBuf::from("/repo/.git"),
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
                branch: "aw/task-1".to_owned(),
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
            root_unconfigured: true,
        };
        let label = coverage_label(&coverage);
        assert!(label.contains("deadbeefcafe(timeout)"));
        assert!(label.contains("budget exhausted"));
        assert!(label.contains("worktree root not configured"));
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
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["schema_version"], 1);
        assert_eq!(structured["kind"], "worktree_page");
        assert!(structured.get("data").is_none());
        assert_eq!(structured["error"]["code"], "invalid_arguments");
        assert_eq!(structured["error"]["outcome"], "blocked");
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
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["error"]["code"], "limit_out_of_range");
        assert_eq!(structured["error"]["outcome"], "blocked");
        assert!(structured.get("data").is_none());
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
        assert_eq!(
            result.structured_content.unwrap()["error"]["code"],
            "cursor_invalid"
        );
    }

    /// One page outcome with every row axis: linked worktrees of one
    /// repository, an unprobed row, an unknown-signal row, a detached row and
    /// a lower-bound size.
    fn sample_outcome() -> ListOutcome {
        let mut probed = sample_row();
        probed.activity = Some(crate::worktree::Activity::StaleCandidate);
        probed.integration = Some(crate::worktree::Integration::Unmerged);
        let mut unknown_pass = sample_row();
        unknown_pass.key = "0123456789ab/task-2".to_owned();
        unknown_pass.path = std::path::PathBuf::from("/tmp/w/demo--0123456789ab/task-2");
        unknown_pass.activity = Some(crate::worktree::Activity::Unknown);
        unknown_pass.integration = Some(crate::worktree::Integration::Unknown);
        unknown_pass.size = None;
        let mut detached = sample_row();
        detached.key = "0123456789ab/task-3".to_owned();
        detached.path = std::path::PathBuf::from("/tmp/w/demo--0123456789ab/task-3");
        detached.branch = None;
        detached.detached = true;
        detached.creator = None;
        detached.activity = None;
        detached.integration = None;
        detached.size = None;
        let mut orphan = sample_row();
        orphan.key = "0123456789ab/leftover".to_owned();
        orphan.path = std::path::PathBuf::from("/tmp/w/demo--0123456789ab/leftover");
        orphan.class = crate::worktree::WorktreeClass::OrphanCandidate;
        orphan.branch = None;
        orphan.activity = None;
        orphan.integration = None;
        orphan.size = None;
        ListOutcome {
            rows: vec![probed, unknown_pass, detached, orphan],
            has_more: true,
            cursor: Some("YXdsaGlzdDE".to_owned()),
            coverage: Coverage {
                repos: 1,
                registry: 1,
                discovered: 0,
                failed: vec![("deadbeefcafe".to_owned(), "timeout")],
                budget_exhausted: false,
                orphan_scan_truncated: false,
                root_unconfigured: false,
            },
            hygiene: crate::service::Hygiene {
                missing: 0,
                orphan: 1,
                removal_started: 0,
                idle: 1,
                stale: 1,
                unmerged: 1,
                large: true,
            },
        }
    }

    #[test]
    fn structured_page_types_every_axis() {
        let outcome = sample_outcome();
        let structured = result_page(&outcome);
        let data = &structured["data"];
        assert_eq!(data["returned"], 4);
        assert_eq!(data["has_more"], true);
        assert_eq!(data["cursor"], "YXdsaGlzdDE");
        // Canonical repository identity, shared by every linked row.
        assert_eq!(data["rows"][0]["repo_id"], "0".repeat(64));
        assert_eq!(data["rows"][0]["repo_common_dir"], "/repo/.git");
        assert_eq!(data["rows"][1]["repo_id"], data["rows"][0]["repo_id"]);
        assert_eq!(
            data["rows"][1]["repo_common_dir"],
            data["rows"][0]["repo_common_dir"]
        );
        // Typed row facts, never display labels.
        let probed = &data["rows"][0];
        assert_eq!(probed["key"], "0123456789ab/task-1");
        assert_eq!(probed["class"], "managed");
        assert_eq!(probed["branch"], "refs/heads/aw/task-1");
        assert_eq!(probed["detached"], false);
        assert!(probed.get("head").is_some());
        assert!(probed["head"].is_null());
        assert_eq!(probed["creator"], "claude-code");
        assert_eq!(probed["removal_started"], false);
        assert_eq!(probed["activity"]["state"], "stale_candidate");
        assert_eq!(probed["activity"]["mtime_based"], true);
        assert_eq!(probed["integration"], "unmerged");
        assert_eq!(probed["size"]["bytes"], 2_u64 << 30);
        assert_eq!(probed["size"]["quality"], "lower_bound");
        // An unknown pass is unknown, never clean or absent.
        assert_eq!(data["rows"][1]["activity"]["state"], "unknown");
        assert_eq!(data["rows"][1]["activity"]["mtime_based"], false);
        assert_eq!(data["rows"][1]["integration"], "unknown");
        assert!(data["rows"][1]["size"].is_null());
        // Rows that were not probed stay explicitly null.
        assert!(data["rows"][2]["activity"].is_null());
        assert!(data["rows"][2]["integration"].is_null());
        assert_eq!(data["rows"][2]["detached"], true);
        assert!(data["rows"][2]["branch"].is_null());
        assert!(data["rows"][2]["creator"].is_null());
        assert!(data["rows"][3]["activity"].is_null());
        assert_eq!(data["rows"][3]["class"], "orphan_candidate");
        // Hygiene and coverage keep their page semantics.
        assert_eq!(data["hygiene"]["orphan"], 1);
        assert_eq!(data["hygiene"]["idle"], 1);
        assert_eq!(data["hygiene"]["stale"], 1);
        assert_eq!(data["hygiene"]["unmerged"], 1);
        assert_eq!(data["hygiene"]["large"], true);
        assert_eq!(data["coverage"]["repos"], 1);
        assert_eq!(data["coverage"]["failed"][0]["repo"], "deadbeefcafe");
        assert_eq!(data["coverage"]["failed"][0]["code"], "timeout");
        assert_eq!(data["coverage"]["budget_exhausted"], false);
        assert_eq!(data["coverage"]["root_unconfigured"], false);
    }

    #[test]
    fn structured_cursor_matches_text_page() {
        let outcome = sample_outcome();
        let reply = page_reply(&templates(), &outcome, false);
        assert_eq!(reply.is_error, Some(false));
        let text = crate::response::first_text(&reply);
        let cursor_line = text
            .lines()
            .find(|l| l.starts_with("Cursor: "))
            .unwrap_or_else(|| panic!("cursor line missing:\n{text}"))
            .trim_start_matches("Cursor: ");
        let data = &reply.structured_content.unwrap()["data"];
        assert_eq!(data["cursor"], cursor_line);
        assert_eq!(data["has_more"], true);
    }

    #[test]
    fn render_failure_keeps_structured_data() {
        // A template set without the page template fails rendering; the reply
        // must stay an error and still carry the usable structured page.
        let empty = Templates::new(&[]).unwrap();
        let reply = page_reply(&empty, &sample_outcome(), false);
        assert_eq!(reply.is_error, Some(true));
        assert_eq!(
            crate::response::first_text(&reply),
            crate::response::READ_FALLBACK
        );
        let structured = reply.structured_content.unwrap();
        assert_eq!(structured["kind"], "worktree_page");
        assert_eq!(structured["data"]["returned"], 4);
        assert_eq!(structured["data"]["rows"].as_array().unwrap().len(), 4);
        assert!(structured.get("error").is_none());
    }

    /// Builds the structured envelope for one outcome through the reply path.
    fn result_page(outcome: &ListOutcome) -> serde_json::Value {
        page_reply(&templates(), outcome, false)
            .structured_content
            .unwrap()
    }
}
