//! Application service: composes Git access, local state and pure policy per call.
//!
//! Every operation is a plain async function over the contracts in
//! [`crate::git`], [`crate::store`] and [`crate::worktree`]. This layer owns
//! outcomes, effects, deadlines, idempotency and recovery; it produces typed
//! results plus stable codes, never MCP prose. Rendering happens in
//! [`crate::tools`] over these results. Git stays authoritative for existence
//! and branch association; state only adds what Git cannot reconstruct.

use crate::git::{self, Checks, CreateSpec, GitError, GitErrorCode, ObserveSpec};
use crate::store::{self, KnownRepo, Layout, StoreError};
use crate::worktree::{
    self, Activity, Advice, Budget, Decision, Fingerprint, Integration, Observation, Policy,
    RECORD_SCHEMA_VERSION, Record, RemovalRequest, RepoId, WorktreeClass, WorktreeName,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Mutation deadline in seconds, including lock waits and verification.
const MUTATION_SECS: u64 = 30;
/// Overall deadline for one list call, shared by every inventory in scope.
const LIST_SECS: u64 = 30;
/// Per-repository inventory deadline in seconds.
const INVENTORY_SECS: u64 = 15;
/// Single observation pass deadline in seconds.
const OBSERVE_SECS: u64 = 20;
/// Discovery pass deadline in seconds.
const DISCOVERY_SECS: u64 = 20;
/// Cap on any single subprocess output.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Cap on directory-walk entries per pass.
const MAX_ENTRIES: usize = 200_000;
/// Cap on orphan-candidate directory entries read per repository.
const MAX_ORPHAN_ENTRIES: usize = 512;
/// Maximum accepted `limit` for list pages.
pub const MAX_PAGE_ROWS: usize = 20;
/// Byte budget reserved for page furniture (header, hygiene, coverage, cursor).
const PAGE_ROW_BUDGET: usize = 7 * 1024;

/// Execution outcome class of a failed service operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ServiceOutcome {
    /// Preconditions or authorization prevent the operation.
    Blocked,
    /// A needed capability is unavailable.
    Unavailable,
    /// An effect may have happened; reconciliation is required.
    OutcomeUnknown,
}

/// Typed service failure; the tools layer turns this into an error reply.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ServiceError {
    /// Outcome class controlling `isError` and the reply form.
    pub outcome: ServiceOutcome,
    /// Stable snake_case code.
    pub code: String,
    /// Bounded human-readable detail.
    pub detail: String,
    /// Optional single safe recovery step.
    pub next: Option<String>,
    /// Reconciliation facts for the unknown-effect form, boxed to keep this
    /// error small in every `Result` signature.
    pub unknown: Option<Box<UnknownEffect>>,
}

/// Operation identity and reconciliation path of an unknown-effect failure.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UnknownEffect {
    /// Operation and target identity.
    pub target: String,
    /// Exact path to inspect before any retry.
    pub path: String,
}

impl ServiceError {
    /// Builds a blocked (refused) failure; no effect was performed.
    pub fn blocked(code: &str, detail: impl Into<String>) -> Self {
        Self {
            outcome: ServiceOutcome::Blocked,
            code: code.to_owned(),
            detail: crate::response::bounded(&detail.into(), 300),
            next: None,
            unknown: None,
        }
    }

    /// Builds an unavailable failure; the needed capability did not run.
    pub fn unavailable(code: &str, detail: impl Into<String>) -> Self {
        Self {
            outcome: ServiceOutcome::Unavailable,
            code: code.to_owned(),
            detail: crate::response::bounded(&detail.into(), 300),
            next: None,
            unknown: None,
        }
    }

    /// Builds an unknown-effect failure naming the exact path to inspect.
    pub fn outcome_unknown(
        code: &str,
        target: &str,
        path: &Path,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            outcome: ServiceOutcome::OutcomeUnknown,
            code: code.to_owned(),
            detail: crate::response::bounded(&detail.into(), 300),
            next: None,
            unknown: Some(Box::new(UnknownEffect {
                target: target.to_owned(),
                path: path.display().to_string(),
            })),
        }
    }

    /// Attaches one safe recovery step (replacing any previous one).
    #[must_use]
    pub fn with_next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(crate::response::bounded(&next.into(), 200));
        self
    }
}

/// Maps a Git failure on a read path; no effect can have been dispatched.
fn git_read_error(error: GitError) -> ServiceError {
    match error.code {
        GitErrorCode::NotARepository => ServiceError::blocked(
            "not_a_repository",
            "path is not a Git repository or worktree",
        ),
        GitErrorCode::Conflict => ServiceError::blocked(
            "conflict",
            format!("Git refused the request: {}", error.code.as_str()),
        ),
        GitErrorCode::NotFound => {
            ServiceError::blocked("not_found", "the named worktree or ref was not found")
        }
        code => ServiceError::unavailable(code.as_str(), "Git operation failed or was not run"),
    }
}

/// Maps a Git failure after a mutation may have been dispatched.
fn git_mutation_error(error: GitError, target: &str, path: &Path) -> ServiceError {
    match error.code {
        GitErrorCode::Timeout | GitErrorCode::OutcomeUnknown => ServiceError::outcome_unknown(
            error.code.as_str(),
            target,
            path,
            "confirmation was not received",
        )
        .with_next(format!("inspect {} before any retry", path.display())),
        code => git_read_error(GitError::new(code, error.detail)),
    }
}

/// Maps a state-store failure; state writes never dispatch Git effects.
fn store_error(error: StoreError) -> ServiceError {
    match error.code {
        store::StoreErrorCode::LockTimeout => ServiceError::blocked(
            "lock_timeout",
            "repository lock could not be acquired within the deadline",
        )
        .with_next("retry after the competing operation finishes"),
        store::StoreErrorCode::RevisionConflict => ServiceError::blocked(
            "revision_conflict",
            "the record changed between read and write",
        )
        .with_next("re-read the worktree and retry with the observed revision"),
        code => ServiceError::unavailable(code.as_str(), "state operation failed or was not run"),
    }
}

/// Per-process application context: policy plus environment-dependent layout.
#[derive(Clone, Debug)]
pub struct Service {
    /// Thresholds and revision used for classification and fingerprints.
    policy: Policy,
}

impl Service {
    /// Builds the service with the default policy, validated once.
    ///
    /// An inverted policy would make `classify` and `assess_removal`
    /// behaviour unspecified, so a violated invariant refuses startup.
    pub fn new() -> Result<Self, &'static str> {
        let policy = Policy::default();
        policy.validate()?;
        Ok(Self { policy })
    }

    /// Resolves the product home and worktree root from the environment.
    fn layout(&self) -> Result<Layout, ServiceError> {
        let env_home = std::env::var_os("AGENT_WORKTREE_HOME").map(PathBuf::from);
        let env_root = std::env::var_os("AGENT_WORKTREE_ROOT").map(PathBuf::from);
        let platform_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let layout =
            store::resolve_layout(env_home, env_root, platform_home).map_err(store_error)?;
        // A relative root would make Git create worktrees inside the working
        // directory or the repository's own administration tree.
        if !layout.root.is_absolute() {
            return Err(ServiceError::blocked(
                "root_not_absolute",
                "the worktree root must be an absolute path",
            )
            .with_next("set AGENT_WORKTREE_ROOT or [storage] root to an absolute path"));
        }
        Ok(layout)
    }

    /// Resolves the repository scope entry for one caller-supplied repo path.
    async fn repo_scope(&self, layout: &Layout, repo: &str) -> Result<RepoScope, ServiceError> {
        let common_dir = git::common_dir(Path::new(repo), &budget(INVENTORY_SECS))
            .await
            .map_err(git_read_error)?;
        let repo_id = RepoId::from_common_dir(&common_dir);
        let (entry, registered) = known_repo_entry(layout, &repo_id, &common_dir)?;
        let integration_ref = (!entry.integration_ref.is_empty()).then_some(entry.integration_ref);
        Ok(RepoScope {
            common_dir,
            repo_id,
            label: entry.label,
            integration_ref,
            registered,
        })
    }

    /// Creates one worktree; replays reconcile instead of duplicating.
    pub async fn create_worktree(&self, args: &CreateArgs) -> Result<CreateOutcome, ServiceError> {
        let name = args.validate()?;
        let layout = self.layout()?;
        let scope = self.repo_scope(&layout, &args.repo).await?;
        let destination = layout
            .root
            .join(worktree::repo_directory(&scope.label, &scope.repo_id))
            .join(name.as_str());
        let key = format!("{}/{}", scope.repo_id.id12(), name.as_str());
        let op_budget = budget(MUTATION_SECS);
        let guard = store::lock_repo(&layout.home, &scope.repo_id, &op_budget)
            .await
            .map_err(store_error)?;

        // Idempotency from Git plus the record, under the repository lock.
        // Git records canonical paths (/private/… on macOS), while the root
        // comes from the environment, so comparisons use the canonical form.
        let record =
            store::read_record(&layout.home, &scope.repo_id, &name).map_err(store_error)?;
        let inventory = git::inventory(&scope.common_dir, &op_budget)
            .await
            .map_err(git_read_error)?;
        // Git records canonical paths (/private/… on macOS) while the root
        // comes from the environment, so match the canonical destination, the
        // literal one, and the record's bound path (captured at creation).
        let destination_key =
            std::fs::canonicalize(&destination).unwrap_or_else(|_| destination.clone());
        let record_path = record.as_ref().map(|record| record.path.clone());
        if let Some(registration) = inventory.iter().find(|r| {
            r.path == destination_key
                || r.path == destination
                || record_path.as_ref().is_some_and(|path| path == &r.path)
        }) {
            // A registration whose path is gone cannot satisfy a replay: the
            // echoed path would not exist. Reconcile through prune instead.
            if registration.prunable.is_some() || !registration.path.try_exists().unwrap_or(false) {
                return Err(ServiceError::blocked(
                    "conflict",
                    "the existing registration's path is gone",
                )
                .with_next("run prune_worktrees, then retry the create"));
            }
            if let Some(record) = &record {
                if record.matches_request(args) {
                    return Ok(CreateOutcome::Noop {
                        key,
                        path: registration.path.clone(),
                        branch: registration.branch.clone(),
                        head: registration.head.clone(),
                        creator: record.creator.clone(),
                        purpose: record.purpose.clone(),
                        created_at: record.created_at,
                    });
                }
                return Err(ServiceError::blocked(
                    "conflict",
                    "worktree exists with a different metadata binding",
                )
                .with_next("inspect the existing worktree, or choose another name"));
            }
            return Err(ServiceError::blocked(
                "conflict",
                "destination directory exists without a matching record",
            )
            .with_next("inspect the directory, then register or remove it explicitly"));
        }
        if record.is_some() {
            return Err(ServiceError::blocked(
                "conflict",
                "a record exists without a matching Git registration",
            )
            .with_next("inspect the record and reconcile before creating"));
        }
        if destination.try_exists().unwrap_or(false) {
            return Err(ServiceError::blocked(
                "conflict",
                "destination path already exists on disk",
            ));
        }

        let spec = CreateSpec {
            name: &name,
            destination: &destination,
            base: args.base.as_deref(),
            branch: args.branch.as_deref(),
            detached: args.detached,
        };
        let created = git::create(&scope.common_dir, &spec, &op_budget)
            .await
            .map_err(|e| git_mutation_error(e, &format!("create_worktree {key}"), &destination))?;

        // Register the known repository; creation is what records repositories.
        // The integration ref is derived from the main worktree's branch when
        // the registry has none yet, and left empty (unknown) when neither
        // source exists — `main` is never assumed.
        let now = unix_now();
        let mut warnings = Vec::new();
        let integration_ref = scope
            .integration_ref
            .clone()
            .or_else(|| derived_integration(&inventory))
            .unwrap_or_default();
        let entry = KnownRepo {
            repo_id: scope.repo_id.as_str().to_owned(),
            common_dir: scope.common_dir.clone(),
            label: scope.label.clone(),
            integration_ref,
            registered_at: now,
        };
        if let Err(error) = store::add_known_repo(&layout.home, &guard, &op_budget, &entry).await {
            warnings.push(format!("registry_write_failed: {}", error.code.as_str()));
        }
        // The record is written after the confirmed effect; a failure here is a
        // warning, not an unknown effect: the worktree itself is confirmed.
        let record = Record {
            schema_version: RECORD_SCHEMA_VERSION,
            repo_id: scope.repo_id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            path: created.path.clone(),
            branch: created.branch.clone(),
            base_ref: args.base.clone(),
            base_oid: Some(created.head.clone()),
            created_at: now,
            creator: crate::response::bounded(&args.creator, 64),
            session: args.session.clone(),
            purpose: Some(args.purpose.clone()),
            revision: 0,
            removal_started: None,
        };
        if let Err(error) = store::replace_record(&layout.home, &guard, &record, 0) {
            warnings.push(format!("record_write_failed: {}", error.code.as_str()));
        }
        Ok(CreateOutcome::Created {
            key,
            path: created.path,
            branch: created.branch,
            head: created.head,
            creator: record.creator,
            purpose: record.purpose,
            created_at: record.created_at,
            warnings,
        })
    }

    /// Lists worktrees across the requested scope with keyset pagination.
    pub async fn list_worktrees(&self, args: &ListArgs) -> Result<ListOutcome, ServiceError> {
        if let Some(repo) = &args.repo {
            validate_repo_path(repo)?;
        }
        let limit = args.limit.unwrap_or(MAX_PAGE_ROWS);
        if limit == 0 || limit > MAX_PAGE_ROWS {
            return Err(ServiceError::blocked(
                "limit_out_of_range",
                format!("limit must be between 1 and {MAX_PAGE_ROWS}"),
            ));
        }
        // Structural cursor decoding precedes any state access so malformed
        // cursors refuse before a scan runs.
        let after = args.cursor.as_deref().map(parse_cursor).transpose()?;
        // One deadline for the whole call; per-repository inventories share it.
        let overall = budget(LIST_SECS);
        let layout = self.layout()?;
        let include_size = args.size.unwrap_or(false);
        let discovery = args.discovery.unwrap_or(true);

        // Resolve the repository scope: one repo, or registry plus discovery.
        let mut scopes = Vec::new();
        let mut registry_count = 0usize;
        let mut discovered_count = 0usize;
        let mut failed = Vec::new();
        let mut budget_exhausted = false;
        if let Some(repo) = &args.repo {
            let scope = self.repo_scope(&layout, repo).await?;
            registry_count = usize::from(scope.registered);
            scopes.push(scope);
        } else {
            let registry = store::read_registry(&layout.home).map_err(store_error)?;
            for entry in &registry {
                if scopes
                    .iter()
                    .any(|s: &RepoScope| s.common_dir == entry.common_dir)
                {
                    continue;
                }
                registry_count += 1;
                scopes.push(RepoScope {
                    repo_id: RepoId::from_common_dir(&entry.common_dir),
                    common_dir: entry.common_dir.clone(),
                    label: entry.label.clone(),
                    integration_ref: (!entry.integration_ref.is_empty())
                        .then_some(entry.integration_ref.clone()),
                    registered: true,
                });
            }
            if discovery {
                let report =
                    store::discover(&layout, &budget(DISCOVERY_SECS)).map_err(store_error)?;
                budget_exhausted = report.budget_exhausted;
                for found in &report.repos {
                    if scopes.iter().any(|s| s.common_dir == found.common_dir) {
                        continue;
                    }
                    discovered_count += 1;
                    let repo_id = RepoId::from_common_dir(&found.common_dir);
                    let (entry, registered) =
                        known_repo_entry(&layout, &repo_id, &found.common_dir)?;
                    scopes.push(RepoScope {
                        repo_id,
                        common_dir: found.common_dir.clone(),
                        label: entry.label,
                        integration_ref: (!entry.integration_ref.is_empty())
                            .then_some(entry.integration_ref),
                        registered,
                    });
                }
            }
        }

        let scope_digest = list_scope_digest(&args.repo, discovery, include_size, &scopes);
        if let Some(key) = &after {
            check_cursor_scope(key, &scope_digest)?;
        }

        // Inventory and classify every registration; orphan candidates are
        // directories under our per-repo root without a registration. The
        // per-repo facts (common dir, effective integration ref) are kept for
        // the cheap page pass below.
        let mut rows: Vec<ListRow> = Vec::new();
        let mut facts: Vec<(String, PathBuf, Option<String>)> = Vec::new();
        let mut orphan_scan_truncated = false;
        for (index, scope) in scopes.iter().enumerate() {
            if Instant::now() >= overall.deadline {
                // Scopes past the call deadline are named, not silently absent.
                for scope in &scopes[index..] {
                    failed.push((scope.repo_id.id12().to_owned(), "deadline_exceeded"));
                }
                budget_exhausted = true;
                break;
            }
            let inventory = match git::inventory(&scope.common_dir, &overall).await {
                Ok(inventory) => inventory,
                Err(error) => {
                    failed.push((scope.repo_id.id12().to_owned(), error.code.as_str()));
                    continue;
                }
            };
            let registered_paths: Vec<PathBuf> = inventory
                .iter()
                .map(|registration| registration.path.clone())
                .collect();
            for registration in &inventory {
                let name = base_name(&registration.path);
                let record = record_for(&layout.home, &scope.repo_id, &name)?;
                let removal_started = record
                    .as_ref()
                    .and_then(|r| r.removal_started.as_ref())
                    .is_some();
                let class = if registration.prunable.is_some()
                    || !registration.path.try_exists().unwrap_or(false)
                {
                    WorktreeClass::Missing
                } else if registration.is_main {
                    WorktreeClass::Main
                } else if record.is_some() {
                    WorktreeClass::Managed
                } else {
                    WorktreeClass::Foreign
                };
                rows.push(ListRow {
                    key: format!("{}/{}", scope.repo_id.id12(), name),
                    repo_id: scope.repo_id.as_str().to_owned(),
                    path: registration.path.clone(),
                    class,
                    branch: registration.branch.clone(),
                    detached: registration.detached,
                    head: registration.head.clone(),
                    creator: record.as_ref().map(|r| r.creator.clone()),
                    removal_started,
                    activity: None,
                    integration: None,
                    size: None,
                });
            }
            facts.push((
                scope.repo_id.as_str().to_owned(),
                scope.common_dir.clone(),
                scope
                    .integration_ref
                    .clone()
                    .or_else(|| derived_integration(&inventory)),
            ));
            let repo_dir = layout
                .root
                .join(worktree::repo_directory(&scope.label, &scope.repo_id));
            let entries = match std::fs::read_dir(&repo_dir) {
                Ok(entries) => Some(entries),
                Err(error) => {
                    // An absent per-repo directory simply has no orphan
                    // candidates; any other read failure is named in
                    // coverage so `orphan=0` is never shown for a directory
                    // that could not be read.
                    if error.kind() != std::io::ErrorKind::NotFound {
                        failed.push((scope.repo_id.id12().to_owned(), "orphan_scan_failed"));
                    }
                    None
                }
            };
            if let Some(entries) = entries {
                for (scanned, entry) in entries.flatten().enumerate() {
                    if scanned == MAX_ORPHAN_ENTRIES {
                        orphan_scan_truncated = true;
                        break;
                    }
                    let path = entry.path();
                    let name = base_name(&path);
                    // Registration identity is the full path, not the base
                    // name, which can repeat across foreign worktrees.
                    let path_key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
                    if registered_paths.contains(&path_key) {
                        continue;
                    }
                    let is_dir = entry
                        .file_type()
                        .map(|t| t.is_dir() && !t.is_symlink())
                        .unwrap_or(false);
                    if !is_dir {
                        continue;
                    }
                    rows.push(ListRow {
                        key: format!("{}/{}", scope.repo_id.id12(), name),
                        repo_id: scope.repo_id.as_str().to_owned(),
                        path,
                        class: WorktreeClass::OrphanCandidate,
                        branch: None,
                        detached: false,
                        head: None,
                        creator: None,
                        removal_started: false,
                        activity: None,
                        integration: None,
                        size: None,
                    });
                }
            }
        }

        // Stable sort by (repo id, path); keyset filter keeps keyset order exact.
        rows.sort_by(|a, b| (&a.repo_id, &a.path).cmp(&(&b.repo_id, &b.path)));
        rows.retain(|row| match &after {
            Some(cursor) => {
                (&row.repo_id, &row.path).cmp(&(&cursor.repo_id, &cursor.path))
                    == std::cmp::Ordering::Greater
            }
            None => true,
        });
        let (mut page, has_more) = match fit_rows(rows, limit) {
            PageFit::Refused => {
                return Err(ServiceError::blocked(
                    "response_too_large",
                    "a single worktree row exceeds the page budget",
                )
                .with_next("list a narrower scope or shorten the worktree path"));
            }
            PageFit::Fit { rows, has_more } => (rows, has_more),
        };
        self.annotate_page(&mut page, &facts, include_size).await;
        let hygiene = self.page_hygiene(&page, include_size);
        let cursor = if has_more {
            page.last()
                .map(|row| encode_cursor(&scope_digest, &row.repo_id, &row.path))
        } else {
            None
        };
        Ok(ListOutcome {
            rows: page,
            has_more,
            cursor,
            coverage: Coverage {
                repos: scopes.len(),
                registry: registry_count,
                discovered: discovered_count,
                failed,
                budget_exhausted,
                orphan_scan_truncated,
            },
            hygiene,
        })
    }

    /// Adds cheap per-row signals to the rows shown on this page.
    ///
    /// Each existing worktree gets one `git::observe` pass with only the
    /// integration probe (plus optional size): activity comes from the cheap
    /// HEAD/index/reflog timestamps — never lsof or a status walk — and one
    /// bounded `merge-base` answers mergedness. All rows share one page
    /// budget; rows whose pass did not finish show `unknown`, never a guess,
    /// and missing/orphan rows are not probed at all.
    async fn annotate_page(
        &self,
        page: &mut [ListRow],
        facts: &[(String, PathBuf, Option<String>)],
        include_size: bool,
    ) {
        let shared = budget(OBSERVE_SECS);
        let now = unix_now();
        let checks = Checks {
            status: false,
            integration: true,
            processes: false,
            submodules: false,
            size: include_size,
        };
        for row in page.iter_mut() {
            if row.class == WorktreeClass::Missing || row.class == WorktreeClass::OrphanCandidate {
                continue;
            }
            let Some((_, common_dir, integration_ref)) =
                facts.iter().find(|(id, _, _)| *id == row.repo_id)
            else {
                continue;
            };
            let observed = git::observe(
                common_dir,
                &observe_spec(&row.path, integration_ref.as_deref(), checks),
                &shared,
            )
            .await;
            match observed {
                Ok(observation) => {
                    row.activity =
                        Some(worktree::classify(&observation, &self.policy, now).activity);
                    row.integration = match observation.integration {
                        crate::worktree::Probe::Known(value) => Some(value),
                        // A failed or degraded probe is unknown, never clean.
                        _ => Some(Integration::Unknown),
                    };
                    if include_size && let crate::worktree::Probe::Known(size) = observation.size {
                        row.size = Some(size);
                    }
                }
                // The page budget ran out mid-pass: unknown, not a guess.
                Err(_) => {
                    row.activity = Some(Activity::Unknown);
                    row.integration = Some(Integration::Unknown);
                }
            }
        }
    }

    /// Hygiene counts over exactly the rows this page collected.
    fn page_hygiene(&self, page: &[ListRow], include_size: bool) -> Hygiene {
        Hygiene {
            missing: page
                .iter()
                .filter(|r| r.class == WorktreeClass::Missing)
                .count(),
            orphan: page
                .iter()
                .filter(|r| r.class == WorktreeClass::OrphanCandidate)
                .count(),
            removal_started: page.iter().filter(|r| r.removal_started).count(),
            // Main checkouts are context rows, not cleanup candidates, so the
            // age, mergedness and size counters cover linked worktrees only.
            // Stale rows are at least 24 h old too, so they count as idle.
            idle: page
                .iter()
                .filter(|r| r.class != WorktreeClass::Main)
                .filter(|r| {
                    matches!(
                        r.activity,
                        Some(Activity::IdleCandidate | Activity::StaleCandidate)
                    )
                })
                .count(),
            stale: page
                .iter()
                .filter(|r| r.class != WorktreeClass::Main)
                .filter(|r| r.activity == Some(Activity::StaleCandidate))
                .count(),
            unmerged: page
                .iter()
                .filter(|r| r.class != WorktreeClass::Main)
                .filter(|r| r.integration == Some(Integration::Unmerged))
                .count(),
            large: include_size
                && page
                    .iter()
                    .filter(|r| r.class != WorktreeClass::Main)
                    .filter(|r| r.large(&self.policy))
                    .count()
                    > 0,
        }
    }

    /// Inspects one worktree with bounded probes; partial when probes degrade.
    pub async fn inspect_worktree(
        &self,
        args: &InspectArgs,
    ) -> Result<InspectOutcome, ServiceError> {
        validate_repo_path(&args.repo)?;
        let layout = self.layout()?;
        let scope = self.repo_scope(&layout, &args.repo).await?;
        let resolved = self
            .resolve_target(&scope, args.name.as_deref(), args.path.as_deref())
            .await?;
        let target = &resolved.target;
        let checks = args.checks();
        let observation = git::observe(
            &scope.common_dir,
            &observe_spec(&target.path, resolved.integration_ref.as_deref(), checks),
            &budget(OBSERVE_SECS),
        )
        .await
        .map_err(git_read_error)?;
        let record = record_for(&layout.home, &scope.repo_id, &target.name)?;
        let mut advice = worktree::classify(&observation, &self.policy, unix_now());
        // classify has no record parameter, so the interrupted-removal warning
        // is added here from the record this call already read.
        if record
            .as_ref()
            .and_then(|r| r.removal_started.as_ref())
            .is_some()
        {
            advice
                .warnings
                .push(crate::worktree::Warning::RemovalStarted);
        }
        let class = if !target.path.try_exists().unwrap_or(false) {
            WorktreeClass::Missing
        } else if target.is_main {
            WorktreeClass::Main
        } else if record.is_some() {
            WorktreeClass::Managed
        } else {
            WorktreeClass::Foreign
        };
        Ok(InspectOutcome {
            key: format!("{}/{}", scope.repo_id.id12(), target.name),
            class,
            observation,
            advice,
            record,
        })
    }

    /// Previews or applies one worktree removal; never forces, never drops branches.
    pub async fn remove_worktree(&self, args: &RemoveArgs) -> Result<RemoveOutcome, ServiceError> {
        validate_repo_path(&args.repo)?;
        let mode = RemoveMode::parse(args.mode.as_str())?;
        if args.name.is_some() == args.path.is_some() {
            return Err(ServiceError::blocked(
                "target_required",
                "exactly one of name or path must be provided",
            ));
        }
        if mode == RemoveMode::Apply && args.fingerprint.is_none() {
            return Err(ServiceError::blocked(
                "fingerprint_required",
                "apply mode requires the fingerprint returned by the preview",
            )
            .with_next("run mode=preview first"));
        }
        let disposable = validate_disposable(args.disposable_paths.as_deref().unwrap_or(&[]))?;
        let allow_unmerged = args.allow_unmerged.unwrap_or(false);
        let layout = self.layout()?;
        let scope = self.repo_scope(&layout, &args.repo).await?;
        let resolved = match self
            .resolve_target(&scope, args.name.as_deref(), args.path.as_deref())
            .await
        {
            Ok(resolved) => resolved,
            // A replayed apply whose worktree is fully gone (no registration,
            // no directory) is a safe no-op, mirroring `git`'s AlreadyAbsent.
            Err(error)
                if mode == RemoveMode::Apply
                    && error.code == "not_found"
                    && !replay_target(&layout, &scope, args)
                        .1
                        .try_exists()
                        .unwrap_or(false) =>
            {
                let (name, path) = replay_target(&layout, &scope, args);
                let mut warnings = Vec::new();
                // A crashed removal can leave a record with removal_started
                // behind while tree and registration are gone; that record
                // would block the name forever, so delete it under the lock.
                if let Some(valid_name) = WorktreeName::parse(&name).ok()
                    && let Ok(Some(record)) = record_for(&layout.home, &scope.repo_id, &name)
                    && record.path == path
                    && record.removal_started.is_some()
                    && let Err(error) = (|| async {
                        let guard =
                            store::lock_repo(&layout.home, &scope.repo_id, &budget(MUTATION_SECS))
                                .await
                                .map_err(store_error)?;
                        store::delete_record(
                            &layout.home,
                            &guard,
                            &scope.repo_id,
                            &valid_name,
                            record.revision,
                        )
                        .map_err(store_error)
                    })()
                    .await
                {
                    warnings.push(format!("record_cleanup_pending: {}", error.code.as_str()));
                }
                return Ok(RemoveOutcome::Applied {
                    key: format!("{}/{}", scope.repo_id.id12(), name),
                    path,
                    branch: None,
                    outcome: RemoveOutcomeKind::AlreadyAbsent,
                    warnings,
                });
            }
            Err(error) => return Err(error),
        };
        let target = resolved.target;
        let key = format!("{}/{}", scope.repo_id.id12(), target.name);
        let record = record_for(&layout.home, &scope.repo_id, &target.name)?;
        // Preview observes everything; apply re-observes under the lock with
        // status checks immediately before dispatch, because git removes an
        // ignored-only worktree without --force and the IgnoredNotDisposable
        // veto from a fresh observation is the only guard.
        let observe_all = Checks {
            status: true,
            integration: true,
            processes: true,
            submodules: true,
            size: false,
        };
        let request = RemovalRequest {
            expected_fingerprint: None,
            disposable_paths: disposable.clone(),
            allow_unmerged,
        };
        if mode == RemoveMode::Preview {
            let observation = git::observe(
                &scope.common_dir,
                &observe_spec(
                    &target.path,
                    resolved.integration_ref.as_deref(),
                    observe_all,
                ),
                &budget(OBSERVE_SECS),
            )
            .await
            .map_err(git_read_error)?;
            let decision =
                worktree::assess_removal(&observation, record.as_ref(), &request, &self.policy);
            return Ok(RemoveOutcome::Preview {
                key,
                path: target.path,
                head: target.head.clone(),
                branch: target.branch.clone(),
                disposable,
                decision,
            });
        }

        let fingerprint = match &args.fingerprint {
            Some(value) => Fingerprint::parse(value).map_err(|e| {
                ServiceError::blocked(e.code(), "fingerprint must be 64 lowercase hex characters")
            })?,
            // Presence was checked before any state access.
            None => return Err(ServiceError::blocked("fingerprint_required", "unreachable")),
        };
        let op_budget = budget(MUTATION_SECS);
        let guard = store::lock_repo(&layout.home, &scope.repo_id, &op_budget)
            .await
            .map_err(store_error)?;
        let observation = git::observe(
            &scope.common_dir,
            &observe_spec(
                &target.path,
                resolved.integration_ref.as_deref(),
                observe_all,
            ),
            &op_budget,
        )
        .await
        .map_err(git_read_error)?;
        let request = RemovalRequest {
            expected_fingerprint: Some(fingerprint),
            disposable_paths: disposable,
            allow_unmerged,
        };
        let decision =
            worktree::assess_removal(&observation, record.as_ref(), &request, &self.policy);
        if !decision.vetoes.is_empty() {
            let codes = decision
                .vetoes
                .iter()
                .map(|v| v.code())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(
                ServiceError::blocked("removal_refused", format!("refused: {codes}"))
                    .with_next("resolve the vetoes, then rerun the preview"),
            );
        }
        let fingerprint = decision.fingerprint.clone().ok_or_else(|| {
            ServiceError::blocked("probe_unknown", "required probes were not known; refusing")
        })?;
        // Record the dispatch marker first so an interrupt stays visible; a
        // foreign worktree without a record has nothing to mark.
        let mut marked_revision = 0;
        if let (Some(valid_name), Some(record)) = (&resolved.valid_name, &record) {
            let marked = store::mark_removal_started(
                &layout.home,
                &guard,
                &scope.repo_id,
                valid_name,
                &fingerprint,
                record.revision,
                unix_now(),
            )
            .map_err(store_error)?;
            marked_revision = marked.revision;
        }

        let branch = target.branch.clone();
        let path = target.path.clone();
        let removed = git::remove(&scope.common_dir, &path, &op_budget)
            .await
            .map_err(|e| git_mutation_error(e, &format!("remove_worktree {key}"), &path))?;
        // The effect is confirmed past this point; cleanup problems are warnings.
        let mut warnings = Vec::new();
        if let Some(valid_name) = &resolved.valid_name
            && let Err(error) = store::delete_record(
                &layout.home,
                &guard,
                &scope.repo_id,
                valid_name,
                marked_revision,
            )
        {
            warnings.push(format!("record_cleanup_failed: {}", error.code.as_str()));
        }
        Ok(RemoveOutcome::Applied {
            key,
            path,
            branch,
            outcome: removed.into(),
            warnings,
        })
    }

    /// Prunes stale registrations repository-wide; dry run by default.
    ///
    /// The dry run is keyset-paginated over the sorted candidates (`cursor`
    /// and `limit`, at most [`MAX_PAGE_ROWS`]) so an oversized preview is
    /// paged through instead of refused; apply stays repository-wide and
    /// prunes every candidate at once.
    pub async fn prune_worktrees(&self, args: &PruneArgs) -> Result<PruneOutcome, ServiceError> {
        validate_repo_path(&args.repo)?;
        let dry_run = args.dry_run.unwrap_or(true);
        let limit = args.limit.unwrap_or(MAX_PAGE_ROWS);
        if limit == 0 || limit > MAX_PAGE_ROWS {
            return Err(ServiceError::blocked(
                "limit_out_of_range",
                format!("limit must be between 1 and {MAX_PAGE_ROWS}"),
            ));
        }
        let after = args.cursor.as_deref().map(parse_prune_cursor).transpose()?;
        let layout = self.layout()?;
        let scope = self.repo_scope(&layout, &args.repo).await?;
        let op_budget = budget(MUTATION_SECS);
        let result = git::prune(&scope.common_dir, dry_run, &op_budget)
            .await
            .map_err(|e| {
                let target = format!("prune_worktrees {}", scope.repo_id.id12());
                // A dry run dispatches no mutation, so it reports read-style.
                if dry_run {
                    git_read_error(e)
                } else {
                    git_mutation_error(e, &target, &scope.common_dir)
                }
            })?;
        let mut warnings = Vec::new();
        let total = result.candidates.len();
        if result.applied {
            // A pruned registration's record is stale (no registration, no
            // directory); remove it so a later create does not hit a phantom
            // conflict. Only records bound to the pruned path are deleted.
            let guard = store::lock_repo(&layout.home, &scope.repo_id, &op_budget)
                .await
                .map_err(store_error)?;
            for candidate in &result.candidates {
                let name = base_name(candidate);
                let Ok(Some(record)) = record_for(&layout.home, &scope.repo_id, &name) else {
                    continue;
                };
                if &record.path != candidate {
                    continue;
                }
                if let Some(valid_name) = WorktreeName::parse(&name).ok()
                    && let Err(error) = store::delete_record(
                        &layout.home,
                        &guard,
                        &scope.repo_id,
                        &valid_name,
                        record.revision,
                    )
                {
                    warnings.push(format!("record_cleanup_failed: {}", error.code.as_str()));
                }
            }
        }
        // The preview pages over stably sorted paths; the apply above pruned
        // every candidate, so it reports the full list with no cursor.
        let (candidates, has_more, cursor) = if result.applied {
            (result.candidates, false, None)
        } else {
            if let Some((cursor_repo, _)) = &after
                && cursor_repo != scope.repo_id.as_str()
            {
                return Err(ServiceError::blocked(
                    "cursor_scope_mismatch",
                    "cursor belongs to a different repository",
                )
                .with_next("request the first page without a cursor"));
            }
            let mut sorted = result.candidates;
            sorted.sort();
            let remaining: Vec<PathBuf> = sorted
                .into_iter()
                .filter(|path| {
                    after
                        .as_ref()
                        .is_none_or(|(_, cursor_path)| path > cursor_path)
                })
                .collect();
            let has_more = remaining.len() > limit;
            let page: Vec<PathBuf> = remaining.into_iter().take(limit).collect();
            let cursor = has_more
                .then(|| {
                    page.last()
                        .map(|path| encode_prune_cursor(&scope.repo_id, path))
                })
                .flatten();
            (page, has_more, cursor)
        };
        Ok(PruneOutcome {
            repo_id: scope.repo_id.id12().to_owned(),
            candidates,
            total,
            has_more,
            cursor,
            applied: result.applied,
            warnings,
        })
    }

    /// Finds one registration by directory name or exact absolute path.
    ///
    /// The resolved target also carries the effective integration ref: the
    /// registry value, else the main worktree's branch, else `None` (unknown).
    async fn resolve_target(
        &self,
        scope: &RepoScope,
        name: Option<&str>,
        path: Option<&str>,
    ) -> Result<ResolvedTarget, ServiceError> {
        let inventory = git::inventory(&scope.common_dir, &budget(INVENTORY_SECS))
            .await
            .map_err(git_read_error)?;
        let found = match (name, path) {
            (Some(name), None) => {
                let validated = WorktreeName::parse(name)
                    .map_err(|e| ServiceError::blocked(e.code(), "worktree name was rejected"))?;
                inventory
                    .iter()
                    .find(|r| base_name(&r.path) == validated.as_str())
            }
            (None, Some(path)) => {
                if path.is_empty() || path.len() > 1024 || path.starts_with('-') {
                    return Err(ServiceError::blocked(
                        "path_invalid",
                        "path must be an absolute path of at most 1024 bytes",
                    ));
                }
                let requested = PathBuf::from(path);
                let canonical =
                    std::fs::canonicalize(&requested).unwrap_or_else(|_| requested.clone());
                inventory
                    .iter()
                    .find(|r| r.path == requested || r.path == canonical)
            }
            _ => {
                return Err(ServiceError::blocked(
                    "target_ambiguous",
                    "provide exactly one of name or path",
                ));
            }
        };
        let registration = found.ok_or_else(|| {
            ServiceError::blocked(
                "not_found",
                "no Git registration matches the target; orphan directories are not inspectable",
            )
        })?;
        let name = base_name(&registration.path);
        Ok(ResolvedTarget {
            valid_name: WorktreeName::parse(&name).ok(),
            integration_ref: scope
                .integration_ref
                .clone()
                .or_else(|| derived_integration(&inventory)),
            target: Target {
                name,
                path: registration.path.clone(),
                head: registration.head.clone(),
                branch: registration.branch.clone(),
                is_main: registration.is_main,
            },
        })
    }
}

/// One repository in a resolved scope.
struct RepoScope {
    repo_id: RepoId,
    common_dir: PathBuf,
    label: String,
    /// Registry-recorded integration ref; `None` when absent or empty, in which
    /// case observers derive it from the main worktree's branch.
    integration_ref: Option<String>,
    /// Whether the registry already knows this repository.
    registered: bool,
}

/// A resolved removal or inspection target.
struct Target {
    name: String,
    path: PathBuf,
    head: Option<String>,
    branch: Option<String>,
    /// Whether the registration is the repository's main checkout.
    is_main: bool,
}

/// Best-effort (name, path) identity for an apply replay that found nothing.
fn replay_target(layout: &Layout, scope: &RepoScope, args: &RemoveArgs) -> (String, PathBuf) {
    // The root is canonicalized when present so the reconstructed path can be
    // compared with the record's bound path regardless of /var vs /private.
    let root = std::fs::canonicalize(&layout.root).unwrap_or_else(|_| layout.root.clone());
    match (&args.name, &args.path) {
        (Some(name), None) => (
            name.clone(),
            root.join(worktree::repo_directory(&scope.label, &scope.repo_id))
                .join(name),
        ),
        (_, Some(path)) => (base_name(Path::new(path)), canonicalize_gone(path)),
        _ => (String::new(), PathBuf::new()),
    }
}

/// Canonicalizes a caller path for comparison with a record's bound path.
///
/// The tree is normally gone by the time a replay runs, so when the path
/// itself cannot be canonicalized its surviving parent is resolved and the
/// file name re-joined; `/var`-style aliases and symlinked roots then compare
/// equal to the canonical path stored at creation. A path with no resolvable
/// parent is returned unchanged.
fn canonicalize_gone(path: &str) -> PathBuf {
    let raw = Path::new(path);
    if let Ok(resolved) = std::fs::canonicalize(raw) {
        return resolved;
    }
    match (
        raw.parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok()),
        raw.file_name(),
    ) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => raw.to_owned(),
    }
}

/// A target plus facts resolved alongside it.
struct ResolvedTarget {
    target: Target,
    /// The target's name when it parses as a valid `WorktreeName`; records can
    /// only exist under valid names, so foreign names need no record lookup.
    valid_name: Option<WorktreeName>,
    /// Effective integration ref for observation; `None` leaves it unknown.
    integration_ref: Option<String>,
}

/// Builds an operation budget with a relative deadline.
fn budget(secs: u64) -> Budget {
    Budget {
        deadline: Instant::now() + Duration::from_secs(secs),
        max_output_bytes: MAX_OUTPUT_BYTES,
        max_entries: MAX_ENTRIES,
    }
}

/// Current unix time in seconds, saturating at 0.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Final path component as a display string.
fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Builds an observation spec for one worktree path.
fn observe_spec<'a>(
    path: &'a Path,
    integration_ref: Option<&'a str>,
    checks: Checks,
) -> ObserveSpec<'a> {
    ObserveSpec {
        worktree_path: path,
        integration_ref,
        checks,
    }
}

/// Registry entry for a repository, deriving a bounded label when unknown.
fn known_repo_entry(
    layout: &Layout,
    repo_id: &RepoId,
    common_dir: &Path,
) -> Result<(KnownRepo, bool), ServiceError> {
    if let Ok(registry) = store::read_registry(&layout.home)
        && let Some(entry) = registry.iter().find(|e| e.repo_id == repo_id.as_str())
    {
        return Ok((entry.clone(), true));
    }
    let label = derive_label(common_dir);
    // Empty means "not derivable yet"; creation fills it from the main branch.
    Ok((
        KnownRepo {
            repo_id: repo_id.as_str().to_owned(),
            common_dir: common_dir.to_owned(),
            label,
            integration_ref: String::new(),
            registered_at: unix_now(),
        },
        false,
    ))
}

/// Reads the record for a display name; `None` when the name cannot address a
/// record (only valid `WorktreeName`s have records) or none is stored.
fn record_for(home: &Path, repo_id: &RepoId, name: &str) -> Result<Option<Record>, ServiceError> {
    match WorktreeName::parse(name) {
        Ok(name) => store::read_record(home, repo_id, &name).map_err(store_error),
        Err(_) => Ok(None),
    }
}

/// Derives the integration ref from the main worktree's branch, short form.
///
/// `None` when the inventory has no attached main branch (bare or detached);
/// integration then stays unknown instead of assuming `main`.
fn derived_integration(inventory: &[crate::worktree::Registration]) -> Option<String> {
    inventory
        .iter()
        .find(|r| r.is_main)
        .and_then(|r| r.branch.as_deref())
        .map(|branch| branch.trim_start_matches("refs/heads/").to_owned())
}

/// Derives a bounded ASCII label from the repository root directory name.
fn derive_label(common_dir: &Path) -> String {
    let root = common_dir.parent().unwrap_or(common_dir);
    let mut label: String = base_name(root)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else if c == '.' {
                '.'
            } else {
                '-'
            }
        })
        .collect();
    label = label.trim_matches(['-', '.']).to_owned();
    label.truncate(32);
    if label.is_empty() {
        label = "repo".to_owned();
    }
    label
}

/// Validates caller-supplied disposable paths: relative, bounded, no traversal.
fn validate_disposable(paths: &[String]) -> Result<Vec<PathBuf>, ServiceError> {
    if paths.len() > 64 {
        return Err(ServiceError::blocked(
            "disposable_paths_invalid",
            "at most 64 disposable paths are accepted",
        ));
    }
    let mut validated = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = path.as_bytes();
        if path.is_empty()
            || bytes.len() > 256
            || path.starts_with('/')
            || path.contains("..")
            || bytes.iter().any(|b| *b < 0x20 || *b == 0x7f)
        {
            return Err(ServiceError::blocked(
                "disposable_paths_invalid",
                "paths must be relative, bounded and free of traversal",
            ));
        }
        validated.push(PathBuf::from(path));
    }
    Ok(validated)
}

/// Result of fitting rows into one page.
#[derive(Clone, Debug)]
pub enum PageFit {
    /// Not even one row fits the page budget; the reply must refuse.
    Refused,
    /// The fitted page and whether more rows remain in scope.
    Fit {
        /// Rows shown, in keyset order.
        rows: Vec<ListRow>,
        /// Whether rows remain after the last shown row.
        has_more: bool,
    },
}

/// Fits at most `limit` rows under the page row-byte budget.
///
/// Keyset order makes a shrunk page safe: the next page resumes exactly after
/// the last shown row, so no row is skipped. The cursor emitted for a page
/// with more rows is page furniture charged against the same budget, so
/// trailing rows are dropped until row bytes plus cursor fit; a single row
/// that cannot fit refuses the page instead of truncating an identifier.
fn fit_rows(rows: Vec<ListRow>, limit: usize) -> PageFit {
    let mut kept = Vec::with_capacity(limit.min(rows.len()));
    let mut used = 0usize;
    let mut has_more = false;
    for row in rows {
        if kept.len() == limit {
            has_more = true;
            break;
        }
        let line = row_line_bytes(&row);
        if used.saturating_add(line) > PAGE_ROW_BUDGET {
            has_more = true;
            break;
        }
        used += line;
        kept.push(row);
    }
    // Charge the encoded cursor of the last shown row against the budget; a
    // long path inflates the cursor past the furniture left beside the rows.
    while has_more
        && let Some(last) = kept.last()
        && used.saturating_add(encoded_cursor_len(&last.repo_id, &last.path)) > PAGE_ROW_BUDGET
        && let Some(dropped) = kept.pop()
    {
        used = used.saturating_sub(row_line_bytes(&dropped));
    }
    if kept.is_empty() && has_more {
        // Rows existed but not even one row plus its cursor fits: refuse.
        return PageFit::Refused;
    }
    PageFit::Fit {
        rows: kept,
        has_more,
    }
}

/// Upper bound of the cursor `encode_cursor` emits for one row position.
///
/// The scope digest is fixed-size, so the length follows from the encoded
/// byte count alone: `awlist1` + 32 digest bytes + repo id + NUL + raw path
/// bytes, base64 at four characters per three bytes.
fn encoded_cursor_len(repo_id: &str, path: &Path) -> usize {
    4 * (7 + 32 + repo_id.len() + 1 + path.as_os_str().as_encoded_bytes().len()).div_ceil(3)
}

/// Upper bound of one rendered list row line in bytes.
fn row_line_bytes(row: &ListRow) -> usize {
    // Worst-case cells: branch fallback `detached` (8), activity
    // `stale_candidate~` (16), integration `unmerged` (8), size
    // `16777216.0 TiB (lower bound)` (28) or `unmeasured` (10), creator ≤ 64.
    row.key.len()
        + row.class_label().len()
        + row.branch.as_deref().map_or(8, str::len)
        + row.creator.as_ref().map_or(1, |c| c.len())
        + 16
        + 8
        + row.size.as_ref().map_or(10, |_| 28)
        + row.path.display().to_string().len()
        // Seven " | " separators plus the newline.
        + 21
        + 1
}
fn encode_cursor(scope_digest: &[u8; 32], repo_id: &str, path: &Path) -> String {
    // The path travels as raw OS bytes so non-UTF-8 worktree names round-trip.
    let mut bytes = Vec::with_capacity(64 + 8 + path.as_os_str().len());
    bytes.extend_from_slice(b"awlist1");
    bytes.extend_from_slice(scope_digest);
    bytes.extend_from_slice(repo_id.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(path.as_os_str().as_encoded_bytes());
    base64_url_encode(&bytes)
}

/// Decoded keyset cursor position.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CursorKey {
    /// Scope digest the cursor was issued under.
    pub digest: [u8; 32],
    /// Repository identity of the last row shown.
    pub repo_id: String,
    /// Worktree path of the last row shown.
    pub path: PathBuf,
}

/// Structurally decodes one cursor without scope knowledge.
fn parse_cursor(encoded: &str) -> Result<CursorKey, ServiceError> {
    let invalid = || ServiceError::blocked("cursor_invalid", "cursor could not be decoded");
    let bytes = base64_url_decode(encoded).ok_or_else(invalid)?;
    if bytes.len() < 8 + 32 + 1 || &bytes[..7] != b"awlist1" {
        return Err(invalid());
    }
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&bytes[7..39]);
    let rest = &bytes[39..];
    let split = rest.iter().position(|b| *b == 0).ok_or_else(invalid)?;
    let repo_id = std::str::from_utf8(&rest[..split]).map_err(|_| invalid())?;
    if RepoId::parse(repo_id).is_err() {
        return Err(invalid());
    }
    // Paths are raw OS bytes, not guaranteed UTF-8; decode them losslessly.
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(&rest[split + 1..]))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(std::str::from_utf8(&rest[split + 1..]).map_err(|_| invalid())?);
    if path.as_os_str().is_empty() {
        return Err(invalid());
    }
    Ok(CursorKey {
        digest,
        repo_id: repo_id.to_owned(),
        path,
    })
}

/// Encodes one prune-preview cursor: repository identity plus last shown path.
fn encode_prune_cursor(repo_id: &RepoId, path: &Path) -> String {
    let mut bytes = Vec::with_capacity(8 + 64 + path.as_os_str().len());
    bytes.extend_from_slice(b"awprune1");
    bytes.extend_from_slice(repo_id.as_str().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(path.as_os_str().as_encoded_bytes());
    base64_url_encode(&bytes)
}

/// Structurally decodes one prune-preview cursor.
fn parse_prune_cursor(encoded: &str) -> Result<(String, PathBuf), ServiceError> {
    let invalid = || ServiceError::blocked("cursor_invalid", "cursor could not be decoded");
    let bytes = base64_url_decode(encoded).ok_or_else(invalid)?;
    if bytes.len() < 8 + 64 + 1 + 1 || &bytes[..8] != b"awprune1" {
        return Err(invalid());
    }
    let rest = &bytes[8..];
    let split = rest.iter().position(|b| *b == 0).ok_or_else(invalid)?;
    let repo_id = std::str::from_utf8(&rest[..split]).map_err(|_| invalid())?;
    if RepoId::parse(repo_id).is_err() {
        return Err(invalid());
    }
    // Paths are raw OS bytes, not guaranteed UTF-8; decode them losslessly.
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(&rest[split + 1..]))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(std::str::from_utf8(&rest[split + 1..]).map_err(|_| invalid())?);
    if path.as_os_str().is_empty() {
        return Err(invalid());
    }
    Ok((repo_id.to_owned(), path))
}

/// Checks a decoded cursor against the digest of the current scope.
fn check_cursor_scope(key: &CursorKey, scope_digest: &[u8; 32]) -> Result<(), ServiceError> {
    if key.digest != scope_digest[..] {
        return Err(ServiceError::blocked(
            "cursor_scope_mismatch",
            "cursor belongs to a different scope",
        )
        .with_next("request the first page without a cursor"));
    }
    Ok(())
}

/// Digest binding a cursor to its scope: filter, flags and repo set.
fn list_scope_digest(
    repo_filter: &Option<String>,
    discovery: bool,
    include_size: bool,
    scopes: &[RepoScope],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"aw-scope-v1");
    hasher.update([u8::from(repo_filter.is_some())]);
    if let Some(repo) = repo_filter {
        hasher.update(repo.as_bytes());
    }
    hasher.update([u8::from(discovery), u8::from(include_size)]);
    let mut ids: Vec<&str> = scopes.iter().map(|s| s.repo_id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    hasher.update((ids.len() as u64).to_le_bytes());
    for id in ids {
        hasher.update((id.len() as u64).to_le_bytes());
        hasher.update(id.as_bytes());
    }
    hasher.finalize().into()
}

/// URL-safe base64 without padding (alphabet A-Z a-z 0-9 - _).
fn base64_url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// Decodes URL-safe base64 without padding; rejects other alphabets.
fn base64_url_decode(text: &str) -> Option<Vec<u8>> {
    fn value(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let bytes = text.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n: u32 = 0;
        for (i, byte) in chunk.iter().enumerate() {
            n |= value(*byte)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Typed tool arguments for `create_worktree`.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct CreateArgs {
    /// Repository root or any worktree inside it.
    pub repo: String,
    /// Worktree name: 1–64 characters of `[a-z0-9-]`, no leading hyphen.
    pub name: String,
    /// Base ref or commit; the repository HEAD when omitted.
    pub base: Option<String>,
    /// Existing branch to check out; never reset to satisfy creation.
    pub branch: Option<String>,
    /// Check out `base` detached instead of creating a branch.
    #[serde(default)]
    pub detached: bool,
    /// Creator harness attribution; attribution, not authentication.
    pub creator: String,
    /// Creating session identifier.
    pub session: Option<String>,
    /// Why the worktree exists, at most 200 characters.
    pub purpose: String,
}

impl CreateArgs {
    /// Validates every bounded field before any effect is considered.
    fn validate(&self) -> Result<WorktreeName, ServiceError> {
        validate_repo_path(&self.repo)?;
        let name = WorktreeName::parse(&self.name)
            .map_err(|e| ServiceError::blocked(e.code(), "worktree name was rejected"))?;
        if let Some(base) = &self.base {
            worktree::validate_branch(base)
                .map_err(|e| ServiceError::blocked(e.code(), "base ref was rejected"))?;
        }
        if let Some(branch) = &self.branch {
            worktree::validate_branch(branch)
                .map_err(|e| ServiceError::blocked(e.code(), "branch was rejected"))?;
        }
        if self.detached && self.branch.is_some() {
            return Err(ServiceError::blocked(
                "invalid_arguments",
                "detached and branch are mutually exclusive",
            ));
        }
        validate_attribution(&self.creator, 64, "creator_invalid")?;
        if let Some(session) = &self.session {
            validate_attribution(session, 128, "session_invalid")?;
        }
        validate_attribution(&self.purpose, 200, "purpose_invalid")?;
        Ok(name)
    }
}

impl Record {
    /// Whether a stored record matches a repeated creation request.
    ///
    /// Every persisted intake field participates: base, branch (in the form
    /// `git::create` reports, without a `refs/heads/` prefix), creator,
    /// session and purpose.
    fn matches_request(&self, args: &CreateArgs) -> bool {
        let requested_branch = if args.detached {
            None
        } else {
            Some(args.branch.clone().unwrap_or_else(|| {
                WorktreeName::parse(&args.name)
                    .map_or_else(|_| String::new(), |n| n.default_branch())
            }))
        };
        self.base_ref == args.base
            && self.branch == requested_branch
            && self.creator == crate::response::bounded(&args.creator, 64)
            && self.session == args.session
            && self.purpose.as_deref() == Some(args.purpose.as_str())
    }
}

/// Typed tool arguments for `list_worktrees`.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct ListArgs {
    /// Restrict the scope to one repository; omit for all known repos.
    pub repo: Option<String>,
    /// Scan configured discovery roots in addition to the registry.
    pub discovery: Option<bool>,
    /// Keyset cursor from a previous page.
    pub cursor: Option<String>,
    /// Rows per page, between 1 and 20.
    pub limit: Option<usize>,
    /// Measure on-disk size of the rows shown on this page.
    pub size: Option<bool>,
}

/// Typed tool arguments for `inspect_worktree`.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct InspectArgs {
    /// Repository root or any worktree inside it.
    pub repo: String,
    /// Worktree directory name.
    pub name: Option<String>,
    /// Absolute worktree path.
    pub path: Option<String>,
    /// Expensive probes to run; defaults are status, integration and processes.
    pub checks: Option<CheckArgs>,
}

/// Per-probe selection for `inspect_worktree`.
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct CheckArgs {
    /// Collect `git status --porcelain=v2 -z --ignored=matching` facts.
    pub status: Option<bool>,
    /// Check ancestry against the integration ref.
    pub integration: Option<bool>,
    /// Look for live processes with cwd inside the tree.
    pub processes: Option<bool>,
    /// Collect submodule dirtiness.
    pub submodules: Option<bool>,
    /// Measure on-disk size.
    pub size: Option<bool>,
}

impl InspectArgs {
    /// Resolves the probe selection, defaulting to the cheap safety probes.
    fn checks(&self) -> Checks {
        match &self.checks {
            Some(args) => Checks {
                status: args.status.unwrap_or(false),
                integration: args.integration.unwrap_or(false),
                processes: args.processes.unwrap_or(false),
                submodules: args.submodules.unwrap_or(false),
                size: args.size.unwrap_or(false),
            },
            None => Checks {
                status: true,
                integration: true,
                processes: true,
                submodules: false,
                size: false,
            },
        }
    }
}

/// Typed tool arguments for `remove_worktree`.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct RemoveArgs {
    /// Repository root or any worktree inside it.
    pub repo: String,
    /// Worktree directory name.
    pub name: Option<String>,
    /// Absolute worktree path.
    pub path: Option<String>,
    /// `preview` (assess and fingerprint) or `apply` (remove under the fingerprint).
    pub mode: String,
    /// Ignored paths approved for deletion, worktree-relative.
    pub disposable_paths: Option<Vec<String>>,
    /// Explicit confirmation allowing removal of an unmerged worktree.
    pub allow_unmerged: Option<bool>,
    /// Fingerprint returned by the preview being applied.
    pub fingerprint: Option<String>,
}

/// Removal modes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RemoveMode {
    /// Assess and fingerprint without effect.
    Preview,
    /// Remove under the expected fingerprint.
    Apply,
}

impl RemoveMode {
    /// Parses the mode argument.
    fn parse(value: &str) -> Result<Self, ServiceError> {
        match value {
            "preview" => Ok(Self::Preview),
            "apply" => Ok(Self::Apply),
            _ => Err(ServiceError::blocked(
                "invalid_mode",
                "mode must be preview or apply",
            )),
        }
    }
}

/// Typed tool arguments for `prune_worktrees`.
#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct PruneArgs {
    /// Repository root or any worktree inside it.
    pub repo: String,
    /// List candidates only; the default.
    pub dry_run: Option<bool>,
    /// Keyset cursor from the previous preview page.
    pub cursor: Option<String>,
    /// Candidates per preview page, between 1 and 20.
    pub limit: Option<usize>,
}

/// Validates a caller-supplied repository path argument.
fn validate_repo_path(repo: &str) -> Result<(), ServiceError> {
    if repo.is_empty() || repo.len() > 1024 || repo.starts_with('-') {
        return Err(ServiceError::blocked(
            "repo_path_invalid",
            "repo must be a path of at most 1024 bytes",
        ));
    }
    Ok(())
}

/// Validates an attribution string: bounded, single-line, printable.
fn validate_attribution(value: &str, max: usize, code: &str) -> Result<(), ServiceError> {
    let printable = value.chars().all(|c| !c.is_control() && c != '\u{7f}');
    if value.is_empty() || value.len() > max || !printable {
        return Err(ServiceError::blocked(
            code,
            format!("value must be 1..={max} printable single-line characters"),
        ));
    }
    Ok(())
}

/// Result of a successful creation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CreateOutcome {
    /// The worktree was created and recorded.
    Created {
        /// Compact tool key `<id12>/<name>`.
        key: String,
        /// Absolute working tree path.
        path: PathBuf,
        /// Branch created or checked out; `None` when detached.
        branch: Option<String>,
        /// Resolved HEAD commit after creation.
        head: String,
        /// Creator attribution stored in the record.
        creator: String,
        /// Purpose stored in the record.
        purpose: Option<String>,
        /// Creation time (unix seconds).
        created_at: u64,
        /// Non-blocking follow-up problems (stable codes).
        warnings: Vec<String>,
    },
    /// The same request already held; no new effect.
    Noop {
        /// Compact tool key `<id12>/<name>`.
        key: String,
        /// Absolute working tree path.
        path: PathBuf,
        /// Branch checked out; `None` when detached.
        branch: Option<String>,
        /// HEAD commit at replay time.
        head: Option<String>,
        /// Creator attribution from the stored record.
        creator: String,
        /// Purpose from the stored record.
        purpose: Option<String>,
        /// Creation time (unix seconds) from the stored record.
        created_at: u64,
    },
}

/// One classified inventory row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ListRow {
    /// Compact tool key `<id12>/<name>`.
    pub key: String,
    /// Full repository identity.
    pub repo_id: String,
    /// Absolute worktree path.
    pub path: PathBuf,
    /// Ownership classification.
    pub class: WorktreeClass,
    /// Full branch ref when attached.
    pub branch: Option<String>,
    /// Whether HEAD is detached.
    pub detached: bool,
    /// HEAD commit when Git reported it.
    pub head: Option<String>,
    /// Record creator when a record backs the row.
    pub creator: Option<String>,
    /// Whether the record's removal was dispatched but never completed.
    pub removal_started: bool,
    /// Mtime-based activity band when the cheap page pass ran; `None` when the
    /// row was not probed (missing or orphan rows).
    pub activity: Option<Activity>,
    /// Mergedness from the cheap page pass; `None` when not probed.
    pub integration: Option<Integration>,
    /// On-disk size when requested and measurable.
    pub size: Option<worktree::Size>,
}

impl ListRow {
    /// Stable label for the ownership class.
    pub fn class_label(&self) -> &'static str {
        match self.class {
            WorktreeClass::Main => "main",
            WorktreeClass::Managed => "managed",
            WorktreeClass::Foreign => "foreign",
            WorktreeClass::Missing => "missing",
            WorktreeClass::OrphanCandidate => "orphan_candidate",
        }
    }

    /// Display label for the branch axis, without the `refs/heads/` prefix.
    pub fn branch_label(&self) -> &str {
        self.branch
            .as_deref()
            .map(crate::response::short_branch)
            .unwrap_or(if self.detached { "detached" } else { "unknown" })
    }

    /// Whether a measured size reaches the policy's large threshold.
    #[must_use]
    pub fn large(&self, policy: &Policy) -> bool {
        self.size
            .as_ref()
            .is_some_and(|size| size.bytes >= policy.size_warning_bytes)
    }
}

/// Hygiene counts over exactly the rows one page collected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hygiene {
    /// Page rows whose registered path is absent.
    pub missing: usize,
    /// Page rows that are directories without a registration.
    pub orphan: usize,
    /// Page rows whose removal was dispatched but never completed.
    pub removal_started: usize,
    /// Page rows idle for at least `Policy::idle_after_secs` (mtime-based);
    /// includes stale rows.
    pub idle: usize,
    /// Page rows stale for at least `Policy::stale_after_secs` (a subset of
    /// `idle`).
    pub stale: usize,
    /// Page rows whose HEAD is not merged into the integration ref.
    pub unmerged: usize,
    /// Whether any measured page row reached the policy's large threshold;
    /// only meaningful when size was requested.
    pub large: bool,
}

/// Scope coverage of one list call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Coverage {
    /// Repositories in the resolved scope.
    pub repos: usize,
    /// Repositories that came from the registry.
    pub registry: usize,
    /// Repositories found by scanning configured roots.
    pub discovered: usize,
    /// Repositories whose inventory failed, with stable codes.
    pub failed: Vec<(String, &'static str)>,
    /// Whether the discovery or call budget ran out before covering the scope.
    pub budget_exhausted: bool,
    /// Whether the per-repo orphan scan hit its entry cap without finishing.
    pub orphan_scan_truncated: bool,
}

/// Result of a successful list call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ListOutcome {
    /// Rows shown on this page, in keyset order.
    pub rows: Vec<ListRow>,
    /// Whether more rows exist in scope after this page.
    pub has_more: bool,
    /// Cursor resuming after the last shown row.
    pub cursor: Option<String>,
    /// Scope coverage facts.
    pub coverage: Coverage,
    /// Hygiene counts over the full collected scope.
    pub hygiene: Hygiene,
}

/// Result of a successful inspection.
pub struct InspectOutcome {
    /// Compact tool key `<id12>/<name>`.
    pub key: String,
    /// Ownership classification.
    pub class: WorktreeClass,
    /// Full observation evidence.
    pub observation: Observation,
    /// Classified advice.
    pub advice: Advice,
    /// Stored record when one exists.
    pub record: Option<Record>,
}

/// Classification label shared with the tools layer.
pub fn class_label(class: WorktreeClass) -> &'static str {
    match class {
        WorktreeClass::Main => "main",
        WorktreeClass::Managed => "managed",
        WorktreeClass::Foreign => "foreign",
        WorktreeClass::Missing => "missing",
        WorktreeClass::OrphanCandidate => "orphan_candidate",
    }
}

/// Activity label shared with the tools layer.
pub fn activity_label(activity: Activity) -> &'static str {
    match activity {
        Activity::Active => "active",
        Activity::Recent => "recent",
        Activity::IdleCandidate => "idle_candidate",
        Activity::StaleCandidate => "stale_candidate",
        Activity::Unknown => "unknown",
    }
}

/// Result of a successful removal call.
pub enum RemoveOutcome {
    /// Assessment without effect.
    Preview {
        /// Compact tool key `<id12>/<name>`.
        key: String,
        /// Exact path that would be removed.
        path: PathBuf,
        /// Current HEAD commit.
        head: Option<String>,
        /// Current branch ref.
        branch: Option<String>,
        /// Approved disposable paths echoed from the request.
        disposable: Vec<PathBuf>,
        /// Vetoes, warnings and fingerprint.
        decision: Decision,
    },
    /// Removal executed.
    Applied {
        /// Compact tool key `<id12>/<name>`.
        key: String,
        /// Path that was removed.
        path: PathBuf,
        /// Branch that was retained.
        branch: Option<String>,
        /// Whether the tree was removed or already absent.
        outcome: RemoveOutcomeKind,
        /// Non-blocking follow-up problems (stable codes).
        warnings: Vec<String>,
    },
}

/// Git-level removal result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoveOutcomeKind {
    /// The working tree and its registration were removed.
    Removed,
    /// Nothing existed to remove; a safe replay is a no-op.
    AlreadyAbsent,
}

impl From<git::RemoveOutcome> for RemoveOutcomeKind {
    fn from(value: git::RemoveOutcome) -> Self {
        match value {
            git::RemoveOutcome::Removed => Self::Removed,
            git::RemoveOutcome::AlreadyAbsent => Self::AlreadyAbsent,
        }
    }
}

/// Result of a successful prune call.
pub struct PruneOutcome {
    /// Repository identity prefix.
    pub repo_id: String,
    /// Registrations eligible (dry run: this page) or pruned (apply: all).
    pub candidates: Vec<PathBuf>,
    /// Total eligible candidates in the repository.
    pub total: usize,
    /// Whether more preview pages remain.
    pub has_more: bool,
    /// Cursor resuming the preview after this page.
    pub cursor: Option<String>,
    /// Whether pruning was applied.
    pub applied: bool,
    /// Non-blocking follow-up problems (stable codes).
    pub warnings: Vec<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Test assertions")]
mod tests {
    use super::*;

    fn scope_digest_for(ids: &[&str]) -> [u8; 32] {
        let scopes: Vec<RepoScope> = ids
            .iter()
            .map(|id| RepoScope {
                repo_id: RepoId::parse(id).unwrap(),
                common_dir: PathBuf::from("/x"),
                label: "repo".to_owned(),
                integration_ref: Some("main".to_owned()),
                registered: true,
            })
            .collect();
        list_scope_digest(&None, true, false, &scopes)
    }

    fn sample_id() -> String {
        RepoId::from_common_dir(Path::new("/repo/.git"))
            .as_str()
            .to_owned()
    }

    /// A managed attached row for page-budget tests.
    fn page_row(key: &str, path: &str) -> ListRow {
        ListRow {
            key: key.to_owned(),
            repo_id: sample_id(),
            path: PathBuf::from(path),
            class: WorktreeClass::Managed,
            branch: Some("refs/heads/aw/x".to_owned()),
            detached: false,
            head: None,
            creator: None,
            removal_started: false,
            activity: None,
            integration: None,
            size: None,
        }
    }

    #[test]
    fn cursor_round_trip_and_scope_mismatch() {
        let id = sample_id();
        let digest = scope_digest_for(&[&id]);
        let cursor = encode_cursor(&digest, &id, Path::new("/root/repo--x/task-1"));
        let decoded = parse_cursor(&cursor).unwrap();
        assert_eq!(decoded.repo_id, id);
        assert_eq!(decoded.path, Path::new("/root/repo--x/task-1"));
        check_cursor_scope(&decoded, &digest).unwrap();

        let second = RepoId::from_common_dir(Path::new("/repo2/.git"))
            .as_str()
            .to_owned();
        let other = scope_digest_for(&[&id, &second]);
        let err = check_cursor_scope(&decoded, &other).unwrap_err();
        assert_eq!(err.code, "cursor_scope_mismatch");
        assert!(err.next.is_some());
    }

    #[test]
    fn cursor_round_trips_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;
        let id = sample_id();
        let digest = scope_digest_for(&[&id]);
        let odd = PathBuf::from(std::ffi::OsStr::from_bytes(b"/w/caf\xe9"));
        let cursor = encode_cursor(&digest, &id, &odd);
        let decoded = parse_cursor(&cursor).unwrap();
        assert_eq!(decoded.path.as_os_str().as_bytes(), b"/w/caf\xe9");
    }

    #[test]
    fn cursor_rejects_garbage() {
        for bad in ["", "!!!!", "AAAA", "awlist1"] {
            assert!(parse_cursor(bad).is_err());
        }
    }

    #[test]
    fn base64_url_round_trip() {
        for bytes in [&b""[..], b"a", b"ab", b"abc", b"abcd", &[0u8, 255, 10, 7]] {
            let encoded = base64_url_encode(bytes);
            assert!(
                encoded
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            );
            assert_eq!(base64_url_decode(&encoded).unwrap(), bytes);
        }
        assert!(base64_url_decode("a===").is_none());
    }

    #[test]
    fn fit_rows_respects_limit_and_budget() {
        let rows = (0..30)
            .map(|i| page_row(&format!("r{i}"), &format!("/w/r{i}")))
            .collect();
        let PageFit::Fit {
            rows: page,
            has_more,
        } = fit_rows(rows, 5)
        else {
            panic!("expected a fitted page");
        };
        assert_eq!(page.len(), 5);
        assert!(has_more);
        assert_eq!(page[0].key, "r0");

        let empty_scope: Vec<ListRow> = Vec::new();
        let PageFit::Fit {
            rows: page,
            has_more,
        } = fit_rows(empty_scope, 5)
        else {
            panic!("empty scope is a page, not a refusal");
        };
        assert!(page.is_empty() && !has_more);

        let long = "/w/".to_owned() + &"p".repeat(PAGE_ROW_BUDGET);
        assert!(matches!(
            fit_rows(vec![page_row("big", &long)], 5),
            PageFit::Refused
        ));
    }

    #[test]
    fn row_line_bytes_upper_binds_the_widest_rendered_line() {
        let mut row = page_row("0123456789ab/task-1", "/tmp/w/demo--0123456789ab/task-1");
        row.branch = None;
        row.detached = true;
        row.creator = Some("c".repeat(64));
        row.activity = Some(Activity::StaleCandidate);
        row.integration = Some(Integration::Unmerged);
        row.size = Some(worktree::Size {
            bytes: u64::MAX,
            quality: worktree::SizeQuality::LowerBound,
        });
        let widest_size = format!("{} (lower bound)", crate::response::human_bytes(u64::MAX));
        let rendered = format!(
            "{} | {} | detached | {} | {}~ | unmerged | {widest_size} | {}\n",
            row.key,
            row.class_label(),
            row.creator.as_deref().unwrap(),
            activity_label(Activity::StaleCandidate),
            row.path.display(),
        );
        assert!(row_line_bytes(&row) >= rendered.len());
        // The unmeasured fallback fits its reserved slot as well.
        row.size = None;
        assert!(row_line_bytes(&row) >= rendered.replace(&widest_size, "unmeasured").len());
    }

    #[test]
    fn fit_rows_charges_the_encoded_cursor() {
        // Long paths inflate the cursor (~4/3 of the path bytes) past the
        // furniture beside the rows, so the page shrinks to make room.
        let rows = (0..30)
            .map(|i| page_row(&format!("r{i}"), &format!("/w/{}{i}", "p".repeat(1020))))
            .collect();
        let PageFit::Fit {
            rows: page,
            has_more,
        } = fit_rows(rows, 20)
        else {
            panic!("expected a fitted page");
        };
        assert!(has_more);
        let used: usize = page.iter().map(row_line_bytes).sum();
        let last = page.last().unwrap();
        assert!(used + encoded_cursor_len(&last.repo_id, &last.path) <= PAGE_ROW_BUDGET);

        // A row that fits alone but whose cursor cannot, with more rows
        // behind it: refuse rather than drop the continuation silently.
        let huge = "/w/".to_owned() + &"p".repeat(6000);
        assert!(matches!(
            fit_rows(vec![page_row("big", &huge), page_row("big2", &huge)], 5),
            PageFit::Refused
        ));
    }

    #[test]
    fn prune_cursor_round_trip() {
        let id = RepoId::from_common_dir(Path::new("/repo/.git"));
        let cursor = encode_prune_cursor(&id, Path::new("/w/repo--x/old-task"));
        let (repo_id, path) = parse_prune_cursor(&cursor).unwrap();
        assert_eq!(repo_id, id.as_str());
        assert_eq!(path, Path::new("/w/repo--x/old-task"));
        assert!(parse_prune_cursor("nonsense").is_err());
        // A list cursor is not a prune cursor.
        let list_cursor = encode_cursor(&[0u8; 32], id.as_str(), Path::new("/w/x"));
        assert!(parse_prune_cursor(&list_cursor).is_err());
    }

    #[test]
    fn derive_label_bounds() {
        assert_eq!(derive_label(Path::new("/x/My.Repo/.git")), "my.repo");
        assert_eq!(derive_label(Path::new("/x/-bad-/.git")), "bad");
        assert_eq!(derive_label(Path::new("/x/-/.git")), "repo");
        let long = "a".repeat(80);
        assert_eq!(
            derive_label(&PathBuf::from(format!("/x/{long}/.git"))).len(),
            32
        );
    }

    #[test]
    fn create_args_validation_codes() {
        let base = |name: &str| CreateArgs {
            repo: "/repo".to_owned(),
            name: name.to_owned(),
            base: None,
            branch: None,
            detached: false,
            creator: "claude-code".to_owned(),
            session: None,
            purpose: "ship it".to_owned(),
        };
        assert_eq!(base("ok-1").validate().unwrap().as_str(), "ok-1");
        assert_eq!(base("BAD").validate().unwrap_err().code, "name_charset");
        assert_eq!(base("-lead").validate().unwrap_err().code, "name_charset");
        let mut detached_branch = base("ok-1");
        detached_branch.detached = true;
        detached_branch.branch = Some("aw/x".to_owned());
        assert_eq!(
            detached_branch.validate().unwrap_err().code,
            "invalid_arguments"
        );
        let mut long_purpose = base("ok-1");
        long_purpose.purpose = "x".repeat(201);
        assert_eq!(long_purpose.validate().unwrap_err().code, "purpose_invalid");
    }

    #[test]
    fn disposable_paths_reject_traversal() {
        assert!(validate_disposable(&["target/".to_owned()]).is_ok());
        assert!(validate_disposable(&["/abs".to_owned()]).is_err());
        assert!(validate_disposable(&["../up".to_owned()]).is_err());
        assert!(validate_disposable(&["".to_owned()]).is_err());
    }
}
