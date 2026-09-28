//! Git subprocess contracts for worktree inventory, observation and mutation.
//!
//! Every function runs Git as a subprocess with an explicit deadline, output cap
//! and NUL-separated parsing. Git is never fetched from, never passed `--force`,
//! and hooks/fsmonitor/prompting are suppressed per call. Bodies are frozen
//! substitutes that return `GitErrorCode::NotImplemented`; no subprocess runs yet.

use crate::worktree::{Budget, Observation, Registration, Size, WorktreeName};
use std::path::{Path, PathBuf};

/// Stable error class for a failed Git operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitErrorCode {
    /// The operation exceeded its deadline.
    Timeout,
    /// The path is not a Git repository or worktree.
    NotARepository,
    /// The target already exists or the request would overwrite existing state.
    Conflict,
    /// The effect may have happened despite the failure; reconcile before retry.
    OutcomeUnknown,
    /// A required worktree, ref or object was not found.
    NotFound,
    /// Git output violated the expected shape.
    InvalidOutput,
    /// The subprocess could not be started or exited with a failure.
    ExecutionFailed,
    /// The contract is not implemented yet.
    NotImplemented,
}

impl GitErrorCode {
    /// Returns the stable snake_case code used in tool replies and probes.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::NotARepository => "not_a_repository",
            Self::Conflict => "conflict",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::NotFound => "not_found",
            Self::InvalidOutput => "invalid_output",
            Self::ExecutionFailed => "execution_failed",
            Self::NotImplemented => "not_implemented",
        }
    }
}

/// Typed Git operation error carrying a stable code and a bounded detail.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitError {
    /// Stable error class.
    pub code: GitErrorCode,
    /// Bounded human-readable detail; never a raw untrusted payload.
    pub detail: String,
}

impl GitError {
    /// Builds an error from a code and detail text.
    #[must_use]
    pub fn new(code: GitErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Builds the substitute error used by the frozen contract bodies.
    #[must_use]
    pub fn not_implemented() -> Self {
        Self::new(GitErrorCode::NotImplemented, "git contract not implemented")
    }
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for GitError {}

/// Resolves the canonical absolute Git common directory for `path`.
///
/// `path` may be a repository root, a linked worktree or inside one. Linked
/// worktrees resolve to their parent repository's common directory, so all
/// worktrees of one repository share the identity derived from it.
pub async fn common_dir(path: &Path, budget: &Budget) -> Result<PathBuf, GitError> {
    let _ = (path, budget);
    Err(GitError::not_implemented())
}

/// Lists worktree registrations via bounded `git worktree list --porcelain -z`.
///
/// Returns raw Git facts only; classification against records happens outside
/// this module. The first entry is the main worktree.
pub async fn inventory(common_dir: &Path, budget: &Budget) -> Result<Vec<Registration>, GitError> {
    let _ = (common_dir, budget);
    Err(GitError::not_implemented())
}

/// Which expensive probes `observe` runs; cheap facts are always collected.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Checks {
    /// Collect `git status --porcelain=v2 -z --ignored=matching` facts.
    pub status: bool,
    /// Check ancestry against the integration ref.
    pub integration: bool,
    /// Look for live processes with cwd inside the tree.
    pub processes: bool,
    /// Collect submodule dirtiness.
    pub submodules: bool,
    /// Measure on-disk size.
    pub size: bool,
}

/// Parameters of one bounded observation pass.
#[derive(Clone, Copy)]
pub struct ObserveSpec<'a> {
    /// Absolute path of the worktree to observe.
    pub worktree_path: &'a Path,
    /// Integration ref for ancestry checks (for example `main`); `None` leaves
    /// integration unknown.
    pub integration_ref: Option<&'a str>,
    /// Selection of expensive probes.
    pub checks: Checks,
}

/// Collects all evidence about one worktree into an `Observation`.
///
/// Probes that are not requested or that fail stay `Probe::NotChecked` or
/// `Probe::Unavailable`; the caller never mistakes unknown for clean.
pub async fn observe(
    common_dir: &Path,
    spec: &ObserveSpec<'_>,
    budget: &Budget,
) -> Result<Observation, GitError> {
    let _ = (common_dir, spec, budget);
    Err(GitError::not_implemented())
}

/// Parameters of one worktree creation.
#[derive(Clone, Copy)]
pub struct CreateSpec<'a> {
    /// Validated worktree name.
    pub name: &'a WorktreeName,
    /// Destination directory `<root>/<label>--<id12>/<name>`; must not exist.
    pub destination: &'a Path,
    /// Base ref or commit; `None` uses the repository HEAD.
    pub base: Option<&'a str>,
    /// Existing branch to check out; `None` creates the default `aw/<name>`.
    /// An existing branch is never reset to satisfy creation.
    pub branch: Option<&'a str>,
    /// Check out `base` detached instead of a branch.
    pub detached: bool,
}

/// Confirmed result of one worktree creation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Created {
    /// Absolute path of the created working tree.
    pub path: PathBuf,
    /// Branch that was created or checked out; `None` when detached.
    pub branch: Option<String>,
    /// Resolved HEAD commit id after creation.
    pub head: String,
}

/// Creates one worktree; never overwrites an existing directory or branch.
pub async fn create(spec: &CreateSpec<'_>, budget: &Budget) -> Result<Created, GitError> {
    let _ = (spec, budget);
    Err(GitError::not_implemented())
}

/// Outcome of one worktree removal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemoveOutcome {
    /// The working tree and its registration were removed.
    Removed,
    /// Nothing existed to remove; a safe replay is a no-op.
    AlreadyAbsent,
}

/// Removes one worktree registration and working tree.
///
/// Never passes `--force` and never deletes the branch. All safety vetoes are
/// checked by the caller under the repository lock before this is dispatched.
pub async fn remove(
    common_dir: &Path,
    worktree_path: &Path,
    budget: &Budget,
) -> Result<RemoveOutcome, GitError> {
    let _ = (common_dir, worktree_path, budget);
    Err(GitError::not_implemented())
}

/// Result of repository-wide worktree pruning.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PruneResult {
    /// Registrations eligible for pruning (dry run) or eligible at apply time.
    pub candidates: Vec<PathBuf>,
    /// True when pruning was applied (`dry_run == false`).
    pub applied: bool,
}

/// Prunes stale worktree registrations for one repository.
///
/// With `dry_run` this only reports candidates. Pruning removes registrations,
/// not branches or existing directories; eligibility is rechecked at apply time
/// because native prune has no exact-entry transaction.
pub async fn prune(
    common_dir: &Path,
    dry_run: bool,
    budget: &Budget,
) -> Result<PruneResult, GitError> {
    let _ = (common_dir, dry_run, budget);
    Err(GitError::not_implemented())
}

/// One live process with cwd at or below the inspected path.
///
/// Only process ids are collected; command lines are deliberately not read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ProcessRef {
    /// Process id of the occupying process.
    pub pid: u32,
}

/// Finds live processes with cwd at or below `path`, via a bounded `lsof` pass.
///
/// Permission failures must be reported as incomplete coverage, not as "none".
pub async fn live_processes_under(
    path: &Path,
    budget: &Budget,
) -> Result<Vec<ProcessRef>, GitError> {
    let _ = (path, budget);
    Err(GitError::not_implemented())
}

/// Measures on-disk size of `path` under an explicit budget.
///
/// Traversal never follows symlinks or crosses filesystems implicitly and counts
/// allocated bytes without double-counting hard links. Budget exhaustion yields
/// `Size::quality == LowerBound`, never a fabricated complete total.
pub async fn measure_size(path: &Path, budget: &Budget) -> Result<Size, GitError> {
    let _ = (path, budget);
    Err(GitError::not_implemented())
}
