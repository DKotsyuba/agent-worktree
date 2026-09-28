//! Git subprocess contracts for worktree inventory, observation and mutation.
//!
//! Every function runs Git as a subprocess with an explicit deadline, output cap
//! and NUL-separated parsing. Git is never fetched from, never passed `--force`,
//! and hooks/fsmonitor/prompting are suppressed per call, while inherited
//! repository-selecting `GIT_*` environment variables are stripped so a call can
//! never be re-bound to the wrong repository. Probes that cannot run report
//! `Unavailable` or `Incomplete`, never a clean-looking fake value.

use crate::worktree::{
    ActivitySignals, Budget, Integration, Observation, Probe, Registration, RepoId, Size,
    SizeQuality, StatusFacts, SubmoduleFacts, WorktreeId, WorktreeName,
};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

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
    let args = s(&["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let out = run_git(&args, Some(path), budget, Op::Read).await?;
    if !out.status.success() {
        return Err(git_failure("git rev-parse", &out));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return Err(GitError::new(
            GitErrorCode::InvalidOutput,
            "git-common-dir printed no path",
        ));
    }
    std::fs::canonicalize(line).map_err(|error| {
        GitError::new(
            GitErrorCode::ExecutionFailed,
            format!("cannot canonicalize common dir: {error}"),
        )
    })
}

/// Lists worktree registrations via bounded `git worktree list --porcelain -z`.
///
/// Returns raw Git facts only; classification against records happens outside
/// this module. The first entry is the main worktree.
pub async fn inventory(common_dir: &Path, budget: &Budget) -> Result<Vec<Registration>, GitError> {
    let args = s(&["worktree", "list", "--porcelain", "-z"]);
    let out = run_git(&args, Some(common_dir), budget, Op::Read).await?;
    if !out.status.success() {
        return Err(git_failure("git worktree list", &out));
    }
    parse_worktree_list(&out.stdout)
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
    let registrations = inventory(common_dir, budget).await?;
    let target = canonicalish(spec.worktree_path);
    let registration = registrations
        .iter()
        .find(|candidate| same_tree(&candidate.path, &target, spec.worktree_path))
        .cloned()
        .ok_or_else(|| {
            GitError::new(
                GitErrorCode::NotFound,
                format!(
                    "{} is not registered as a worktree of this repository",
                    spec.worktree_path.display()
                ),
            )
        })?;
    let name = registration
        .path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();
    let gitdir = worktree_gitdir(common_dir, &registration, budget).await;
    let activity_signals = activity_signals(gitdir.as_deref());

    let mut status_probe = Probe::NotChecked;
    let mut submodules_probe = Probe::NotChecked;
    if spec.checks.status || spec.checks.submodules {
        let args = s(&["status", "--porcelain=v2", "-z", "--ignored=matching"]);
        let attempt = run_git(&args, Some(spec.worktree_path), budget, Op::Read).await;
        let failure = match &attempt {
            Ok(out) if out.status.success() => {
                let StatusParse { facts, submodules } = parse_status(&out.stdout);
                if spec.checks.status {
                    status_probe = Probe::Known(facts);
                }
                if spec.checks.submodules {
                    submodules_probe = Probe::Known(submodules);
                }
                None
            }
            Ok(out) => Some(git_failure("git status", out)),
            Err(error) => Some(error.clone()),
        };
        if let Some(error) = failure {
            if spec.checks.status {
                status_probe = unavailable(&error);
            }
            if spec.checks.submodules {
                submodules_probe = unavailable(&error);
            }
        }
    }

    let integration_probe = if !spec.checks.integration {
        Probe::NotChecked
    } else if spec
        .integration_ref
        .is_none_or(|value| value.is_empty() || value.starts_with('-'))
    {
        Probe::Known(Integration::Unknown)
    } else {
        let mut args = s(&["merge-base", "--is-ancestor", "HEAD"]);
        args.push(OsString::from(spec.integration_ref.unwrap_or_default()));
        match run_git(&args, Some(spec.worktree_path), budget, Op::Read).await {
            Ok(out) if out.status.success() => Probe::Known(Integration::AncestorMerged),
            Ok(out) if out.status.code() == Some(1) => Probe::Known(Integration::Unmerged),
            Ok(out) => {
                let lower = out.stderr_line().to_ascii_lowercase();
                if lower.contains("not a valid object name") || lower.contains("unknown revision") {
                    Probe::Known(Integration::Unknown)
                } else {
                    unavailable(&git_failure("git merge-base", &out))
                }
            }
            Err(error) => unavailable(&error),
        }
    };

    let live_processes = if spec.checks.processes {
        match live_processes_under(spec.worktree_path, budget).await {
            Ok(pids) => Probe::Known(!pids.is_empty()),
            Err(error) => unavailable(&error),
        }
    } else {
        Probe::NotChecked
    };

    let size = if spec.checks.size {
        match measure_size(spec.worktree_path, budget).await {
            Ok(measured) => Probe::Known(measured),
            Err(error) => unavailable(&error),
        }
    } else {
        Probe::NotChecked
    };

    Ok(Observation {
        id: WorktreeId {
            repo: RepoId::from_common_dir(common_dir),
            name,
        },
        observed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
        registration,
        activity_signals,
        live_processes,
        status: status_probe,
        submodules: submodules_probe,
        integration: integration_probe,
        size,
    })
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

/// Creates one worktree of the repository at `common_dir`.
///
/// Never overwrites an existing directory or branch: an existing destination,
/// an existing target branch or a branch already checked out elsewhere is a
/// `Conflict`, and combining `base` with an existing `branch` is refused too
/// (Git checks a branch out as-is; a base would be silently ignored).
/// Creation is a mutation, so a timeout after dispatch reports
/// `OutcomeUnknown` instead of implying nothing happened.
pub async fn create(
    common_dir: &Path,
    spec: &CreateSpec<'_>,
    budget: &Budget,
) -> Result<Created, GitError> {
    if let Some(base) = spec.base {
        ensure_ref_arg(base)?;
    }
    if let Some(branch) = spec.branch {
        ensure_ref_arg(branch)?;
    }
    if !spec.detached && matches!((spec.base, spec.branch), (Some(_), Some(_))) {
        return Err(GitError::new(
            GitErrorCode::Conflict,
            "base cannot be combined with checking out an existing branch",
        ));
    }
    if std::fs::symlink_metadata(spec.destination).is_ok() {
        return Err(GitError::new(
            GitErrorCode::Conflict,
            format!("destination {} already exists", spec.destination.display()),
        ));
    }
    if let Some(parent) = spec.destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            GitError::new(
                GitErrorCode::ExecutionFailed,
                format!("cannot create {}: {error}", parent.display()),
            )
        })?;
    }
    let mut args = s(&["worktree", "add", "-q"]);
    if spec.detached {
        args.push("--detach".into());
        args.push(p(spec.destination));
        if let Some(base) = spec.base {
            args.push(OsString::from(base));
        }
    } else if let Some(branch) = spec.branch {
        args.push(p(spec.destination));
        args.push(OsString::from(branch));
    } else {
        args.push("-b".into());
        args.push(OsString::from(spec.name.default_branch()));
        args.push(p(spec.destination));
        if let Some(base) = spec.base {
            args.push(OsString::from(base));
        }
    }
    let out = run_git(&args, Some(common_dir), budget, Op::Mutation).await?;
    if !out.status.success() {
        return Err(git_failure("git worktree add", &out));
    }
    let head_args = s(&["rev-parse", "--verify", "HEAD"]);
    match run_git(&head_args, Some(spec.destination), budget, Op::Read).await {
        Ok(out) if out.status.success() && !out.stdout.is_empty() => Ok(Created {
            path: std::fs::canonicalize(spec.destination)
                .unwrap_or_else(|_| spec.destination.to_path_buf()),
            branch: if spec.detached {
                None
            } else {
                Some(spec.branch.map_or_else(
                    || spec.name.default_branch(),
                    std::borrow::ToOwned::to_owned,
                ))
            },
            head: String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        }),
        _ => Err(GitError::new(
            GitErrorCode::OutcomeUnknown,
            "worktree was created but its HEAD could not be verified",
        )),
    }
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
/// checked by the caller under the repository lock before this is dispatched;
/// a Git refusal is reported as `Conflict` with Git's own message, and a path
/// that is already gone and unregistered is `AlreadyAbsent`.
///
/// Callers MUST veto ignored files outside the approved disposable paths
/// before dispatch: Git itself happily removes a worktree whose only content
/// is ignored files, so this function cannot be the guard for them.
pub async fn remove(
    common_dir: &Path,
    worktree_path: &Path,
    budget: &Budget,
) -> Result<RemoveOutcome, GitError> {
    let registrations = inventory(common_dir, budget).await?;
    let target = canonicalish(worktree_path);
    let registered = registrations
        .iter()
        .any(|candidate| same_tree(&candidate.path, &target, worktree_path));
    if !registered && std::fs::symlink_metadata(worktree_path).is_err() {
        return Ok(RemoveOutcome::AlreadyAbsent);
    }
    let mut args = s(&["worktree", "remove"]);
    args.push(p(worktree_path));
    let out = run_git(&args, Some(common_dir), budget, Op::Mutation).await?;
    if out.status.success() {
        Ok(RemoveOutcome::Removed)
    } else {
        Err(git_failure("git worktree remove", &out))
    }
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
    let registrations = inventory(common_dir, budget).await?;
    // Capture the admin-name → worktree-path mapping before pruning deletes
    // the admin entries it removes.
    let admins = admin_map(common_dir);
    let mut args = s(&["worktree", "prune"]);
    if dry_run {
        args.push("--dry-run".into());
    }
    args.push("-v".into());
    let out = run_git(&args, Some(common_dir), budget, Op::Mutation).await?;
    if !out.status.success() {
        return Err(git_failure("git worktree prune", &out));
    }
    // `git worktree prune -v` reports on stderr, unlike most Git commands.
    let text = String::from_utf8_lossy(&out.stderr);
    let candidates = text
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Removing worktrees/")?;
            // Split at the last separator; prune reasons may contain colons.
            let name = rest.rsplit_once(": ")?.0;
            resolve_worktree_path(&admins, &registrations, name)
        })
        .collect();
    Ok(PruneResult {
        candidates,
        applied: !dry_run,
    })
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
/// Permission failures must be reported as incomplete coverage, not as "none":
/// any `lsof` failure or permission warning is an error, so callers treat
/// process coverage as unknown rather than empty.
pub async fn live_processes_under(
    path: &Path,
    budget: &Budget,
) -> Result<Vec<ProcessRef>, GitError> {
    let base = canonicalish(path);
    let args = s(&["-d", "cwd", "-Fpn"]);
    let out = match run_captured("lsof", &args, None, budget, Op::Read).await {
        Err(error) if is_missing_program(&error) => {
            run_captured("/usr/sbin/lsof", &args, None, budget, Op::Read).await?
        }
        other => other?,
    };
    if !out.status.success() {
        return Err(GitError::new(
            GitErrorCode::ExecutionFailed,
            format!("lsof failed: {}", out.stderr_line()),
        ));
    }
    let stderr = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
    if stderr.contains("operation not permitted") || stderr.contains("permission denied") {
        return Err(GitError::new(
            GitErrorCode::ExecutionFailed,
            "lsof process coverage incomplete: permission denied",
        ));
    }
    let mut pids: Vec<ProcessRef> = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut current: Option<u32> = None;
    for line in out.stdout.split(|&byte| byte == b'\n') {
        if let Some(pid) = line.strip_prefix(b"p".as_slice()) {
            if let Some(value) = std::str::from_utf8(pid).ok().and_then(|t| t.parse().ok()) {
                current = Some(value);
            }
        } else if let (Some(pid), Some(cwd)) = (current, line.strip_prefix(b"n".as_slice()))
            && let Ok(text) = std::str::from_utf8(cwd)
        {
            current = None;
            if Path::new(text).starts_with(&base) && seen.insert(pid) {
                pids.push(ProcessRef { pid });
            }
        }
    }
    Ok(pids)
}

/// Measures on-disk size of `path` under an explicit budget.
///
/// Traversal never follows symlinks or crosses filesystems implicitly and counts
/// allocated bytes without double-counting hard links. Budget exhaustion yields
/// `Size::quality == LowerBound`, never a fabricated complete total.
pub async fn measure_size(path: &Path, budget: &Budget) -> Result<Size, GitError> {
    if Instant::now() >= budget.deadline {
        return Err(GitError::new(
            GitErrorCode::Timeout,
            "budget deadline already reached",
        ));
    }
    let ttl = budget.deadline.saturating_duration_since(Instant::now());
    let root = std::fs::symlink_metadata(path).map_err(|_| {
        GitError::new(
            GitErrorCode::NotFound,
            format!("{} does not exist", path.display()),
        )
    })?;
    if !root.is_dir() {
        return Ok(Size {
            bytes: allocated(&root),
            quality: SizeQuality::Complete,
        });
    }
    // The walk runs on the blocking pool so it cannot stall the runtime.
    let walk_root = path.to_path_buf();
    let deadline = budget.deadline;
    let max_entries = budget.max_entries;
    let walk =
        tokio::task::spawn_blocking(move || size_walk(&root, &walk_root, deadline, max_entries));
    match timeout(ttl, walk).await {
        Ok(Ok(size)) => Ok(size),
        Ok(Err(error)) => Err(GitError::new(
            GitErrorCode::ExecutionFailed,
            format!("size walk failed: {error}"),
        )),
        Err(_) => Err(GitError::new(
            GitErrorCode::Timeout,
            "size walk exceeded the operation deadline",
        )),
    }
}

/// Synchronous bounded directory walk backing `measure_size`.
fn size_walk(root: &std::fs::Metadata, path: &Path, deadline: Instant, max_entries: usize) -> Size {
    let root_device = root.dev();
    let mut bytes = allocated(root);
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut stack = vec![path.to_path_buf()];
    let mut entries = 0usize;
    let mut complete = true;
    'walk: while let Some(dir) = stack.pop() {
        let reader = match std::fs::read_dir(&dir) {
            Ok(reader) => reader,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        for entry in reader {
            if Instant::now() >= deadline || entries >= max_entries {
                complete = false;
                break 'walk;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            entries += 1;
            let meta = match entry.metadata() {
                Ok(meta) => meta,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if meta.dev() != root_device {
                continue;
            }
            if meta.is_dir() {
                bytes += allocated(&meta);
                stack.push(entry.path());
            } else if meta.nlink() > 1 {
                if seen_inodes.insert((meta.dev(), meta.ino())) {
                    bytes += allocated(&meta);
                }
            } else {
                bytes += allocated(&meta);
            }
        }
    }
    Size {
        bytes,
        quality: if complete {
            SizeQuality::Complete
        } else {
            SizeQuality::LowerBound
        },
    }
}

/// Whether a subprocess call is a read or an already-dispatched mutation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    /// Read-only call; exceeding the deadline is a plain `Timeout`.
    Read,
    /// Mutation whose effect may have started; a timeout is `OutcomeUnknown`.
    Mutation,
}

/// Completed subprocess result with both captured streams.
struct RunOutput {
    /// Exit status of the subprocess.
    status: ExitStatus,
    /// Raw standard output, at most `Budget::max_output_bytes` long.
    stdout: Vec<u8>,
    /// Raw standard error, at most `Budget::max_output_bytes` long.
    stderr: Vec<u8>,
}

impl RunOutput {
    /// First non-empty stderr line, bounded for use as error detail.
    fn stderr_line(&self) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        let mut line = text
            .lines()
            .map(str::trim)
            .find(|candidate| !candidate.is_empty())
            .unwrap_or("")
            .to_owned();
        if line.len() > 200 {
            line.truncate(200);
        }
        line
    }
}

/// Builds the argument vector prefix shared by every Git call.
fn s(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// Converts a path into a subprocess argument.
fn p(path: &Path) -> OsString {
    path.as_os_str().to_os_string()
}

/// Runs one Git call with hooks, fsmonitor and prompting suppressed.
///
/// The per-call configuration (`core.hooksPath=/dev/null`,
/// `core.fsmonitor=false`) and environment (`GIT_TERMINAL_PROMPT=0`,
/// `GIT_OPTIONAL_LOCKS=0`, `LC_ALL=C`) follow the Git subprocess rules in
/// `docs/architecture.md`.
async fn run_git(
    args: &[OsString],
    cwd: Option<&Path>,
    budget: &Budget,
    op: Op,
) -> Result<RunOutput, GitError> {
    let mut argv: Vec<OsString> = s(&[
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
    ]);
    argv.extend(args.iter().cloned());
    run_captured("git", &argv, cwd, budget, op).await
}

/// Runs one bounded subprocess: deadline, output cap and kill on drop.
///
/// Repository-selecting `GIT_*` variables inherited from this process are
/// removed: with, say, `GIT_DIR` exported by the caller's environment, every
/// invocation would silently bind to the wrong repository.
async fn run_captured(
    program: &str,
    argv: &[OsString],
    cwd: Option<&Path>,
    budget: &Budget,
    op: Op,
) -> Result<RunOutput, GitError> {
    if let Some(dir) = cwd
        && std::fs::symlink_metadata(dir).is_err()
    {
        return Err(GitError::new(
            GitErrorCode::NotARepository,
            format!("working directory {} is missing", dir.display()),
        ));
    }
    if Instant::now() >= budget.deadline {
        return Err(GitError::new(
            GitErrorCode::Timeout,
            "budget deadline already reached",
        ));
    }
    let ttl = budget.deadline.saturating_duration_since(Instant::now());
    let mut command = Command::new(program);
    command
        .args(argv)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(program, &error))?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let cap = budget.max_output_bytes;
    let work = async {
        let (stdout, stderr) = tokio::join!(
            read_capped(stdout_pipe.as_mut(), cap),
            read_capped(stderr_pipe.as_mut(), cap)
        );
        let streams = match (stdout, stderr) {
            (Ok(out), Ok(err)) => (out, err),
            (Err(error), _) | (_, Err(error)) => return Err(error),
        };
        let status = child.wait().await.map_err(|error| {
            GitError::new(
                GitErrorCode::ExecutionFailed,
                format!("{program} wait failed: {error}"),
            )
        })?;
        Ok((streams.0, streams.1, status))
    };
    match timeout(ttl, work).await {
        Ok(Ok((stdout, stderr, status))) => Ok(RunOutput {
            status,
            stdout,
            stderr,
        }),
        Ok(Err(error)) => {
            // The child is killed when it is dropped with the async block.
            // Anything failing after spawn (pipe read, cap, wait) may already
            // have taken effect for a mutation, so mutations never report a
            // definite read-style failure here.
            let code = if op == Op::Mutation {
                GitErrorCode::OutcomeUnknown
            } else {
                error.code
            };
            Err(GitError {
                code,
                detail: format!("{program}: {}", error.detail),
            })
        }
        Err(_) => Err(GitError::new(
            match op {
                Op::Read => GitErrorCode::Timeout,
                Op::Mutation => GitErrorCode::OutcomeUnknown,
            },
            format!("{program} exceeded the operation deadline"),
        )),
    }
}

/// Reads a pipe to EOF, failing loudly when the cap would be exceeded.
async fn read_capped<R: AsyncRead + Unpin>(
    pipe: Option<&mut R>,
    cap: usize,
) -> Result<Vec<u8>, GitError> {
    let Some(pipe) = pipe else {
        return Ok(Vec::new());
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = pipe.read(&mut chunk).await.map_err(|error| {
            GitError::new(
                GitErrorCode::ExecutionFailed,
                format!("read failed: {error}"),
            )
        })?;
        if read == 0 {
            return Ok(buf);
        }
        if buf.len() + read > cap {
            return Err(GitError::new(
                GitErrorCode::InvalidOutput,
                format!("output exceeded the {cap}-byte cap"),
            ));
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

/// Builds the error for a subprocess that could not be started.
fn spawn_error(program: &str, error: &std::io::Error) -> GitError {
    if error.kind() == std::io::ErrorKind::NotFound {
        GitError::new(
            GitErrorCode::ExecutionFailed,
            format!("{program} executable not found"),
        )
    } else {
        GitError::new(
            GitErrorCode::ExecutionFailed,
            format!("{program} failed to start: {error}"),
        )
    }
}

/// True when a spawn error means the program is not installed.
fn is_missing_program(error: &GitError) -> bool {
    error.code == GitErrorCode::ExecutionFailed && error.detail.contains("not found")
}

/// Maps a failed Git exit to the stable error class using Git's own message.
fn git_failure(context: &str, out: &RunOutput) -> GitError {
    let line = out.stderr_line();
    let lower = line.to_ascii_lowercase();
    let code = if lower.contains("not a git repository") {
        GitErrorCode::NotARepository
    } else if lower.contains("already exists")
        || lower.contains("already used")
        || lower.contains("already checked out")
        || lower.contains("main working tree")
        || lower.contains("modified or untracked files")
        || lower.contains("is not a working tree")
        || lower.contains("is locked")
    {
        GitErrorCode::Conflict
    } else if lower.contains("invalid reference")
        || lower.contains("unknown revision")
        || lower.contains("not a valid object name")
        || lower.contains("bad object")
    {
        GitErrorCode::NotFound
    } else {
        GitErrorCode::ExecutionFailed
    };
    GitError::new(code, format!("{context}: {line}"))
}

/// Rejects caller strings that Git would parse as options instead of refs.
fn ensure_ref_arg(value: &str) -> Result<(), GitError> {
    if value.is_empty() || value.starts_with('-') || value.contains('\0') {
        return Err(GitError::new(
            GitErrorCode::Conflict,
            "ref argument is empty or starts with '-'",
        ));
    }
    Ok(())
}

/// Canonical form of `path` when it exists on disk, else the path as given.
fn canonicalish(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// True when a Git-recorded path names the same tree as the observed target.
fn same_tree(candidate: &Path, target: &Path, literal: &Path) -> bool {
    candidate == target || candidate == literal || canonicalish(candidate) == target
}

/// Marks a probe unavailable with the stable code of `error`.
fn unavailable<T>(error: &GitError) -> Probe<T> {
    Probe::Unavailable {
        code: error.code.as_str().to_owned(),
    }
}

/// Resolves the per-worktree Git directory holding HEAD, index and reflog.
///
/// The main worktree uses the common directory directly; linked worktrees ask
/// Git. When the working tree is missing and Git can no longer answer, the
/// `worktrees/<name>` admin layout is used only if its `gitdir` file still
/// points back at this registration's working tree; otherwise `None` leaves
/// the activity signals unknown.
async fn worktree_gitdir(
    common_dir: &Path,
    registration: &Registration,
    budget: &Budget,
) -> Option<PathBuf> {
    if registration.is_main {
        return Some(common_dir.to_path_buf());
    }
    let args = s(&["rev-parse", "--path-format=absolute", "--git-dir"]);
    if let Ok(out) = run_git(&args, Some(&registration.path), budget, Op::Read).await
        && out.status.success()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(line) = text.lines().next() {
            let parsed = PathBuf::from(line.trim());
            if parsed.is_absolute() {
                return Some(parsed);
            }
        }
    }
    let name = registration.path.file_name()?;
    let candidate = common_dir.join("worktrees").join(name);
    let recorded = std::fs::read_to_string(candidate.join("gitdir")).ok()?;
    (Path::new(recorded.trim()) == registration.path).then_some(candidate)
}

/// Reads the cheap HEAD/index mtimes and the latest reflog timestamp.
fn activity_signals(gitdir: Option<&Path>) -> ActivitySignals {
    let Some(gitdir) = gitdir else {
        return ActivitySignals::default();
    };
    ActivitySignals {
        head_mtime: mtime_secs(&gitdir.join("HEAD")),
        index_mtime: mtime_secs(&gitdir.join("index")),
        last_reflog_entry: last_reflog_secs(&gitdir.join("logs").join("HEAD")),
    }
}

/// Unix-second mtime of one file, `None` when the file or clock fails.
fn mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

/// Timestamp inside the LAST entry of a HEAD reflog, not the file's mtime.
///
/// Reads at most the final 4 KiB of the file, so arbitrarily long-lived
/// worktrees with huge reflogs stay cheap; any error yields `None`.
fn last_reflog_secs(path: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(4096);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = vec![0u8; (len - start) as usize];
    file.read_exact(&mut tail).ok()?;
    let mut lines = tail.split(|&byte| byte == b'\n');
    if start > 0 {
        // After seeking mid-file the first chunk line may be partial.
        lines.next()?;
    }
    let line = lines.rev().find(|candidate| !candidate.is_empty())?;
    let upto_message = &line[..line
        .iter()
        .position(|&byte| byte == b'\t')
        .unwrap_or(line.len())];
    // Entry shape: <old> <new> <name> <email> <timestamp> <tz>\t<message>
    let fields: Vec<&[u8]> = upto_message
        .split(|&byte| byte == b' ')
        .filter(|field| !field.is_empty())
        .collect();
    std::str::from_utf8(fields.get(fields.len().checked_sub(2)?)?)
        .ok()?
        .parse()
        .ok()
}

/// Parses `git worktree list --porcelain -z` bytes into registrations.
fn parse_worktree_list(bytes: &[u8]) -> Result<Vec<Registration>, GitError> {
    let mut registrations: Vec<Registration> = Vec::new();
    let mut current: Option<Registration> = None;
    for token in bytes.split(|&byte| byte == 0) {
        if token.is_empty() {
            if let Some(registration) = current.take() {
                registrations.push(registration);
            }
            continue;
        }
        let (key, value) = match token.iter().position(|&byte| byte == b' ') {
            Some(index) => (&token[..index], &token[index + 1..]),
            None => (token, &[][..]),
        };
        match key {
            b"worktree" => {
                if let Some(registration) = current.take() {
                    registrations.push(registration);
                }
                current = Some(Registration {
                    path: PathBuf::from(OsStr::from_bytes(value)),
                    head: None,
                    branch: None,
                    detached: false,
                    bare: false,
                    locked: None,
                    prunable: None,
                    is_main: false,
                });
            }
            b"HEAD" => {
                if let Some(registration) = current.as_mut() {
                    let text = String::from_utf8_lossy(value);
                    // Git reports the null oid while a branch is unborn.
                    registration.head = if text.chars().all(|c| c == '0') {
                        None
                    } else {
                        Some(text.into_owned())
                    };
                }
            }
            b"branch" => {
                if let Some(registration) = current.as_mut() {
                    registration.branch = Some(String::from_utf8_lossy(value).into_owned());
                }
            }
            b"detached" => {
                if let Some(registration) = current.as_mut() {
                    registration.detached = true;
                }
            }
            b"bare" => {
                if let Some(registration) = current.as_mut() {
                    registration.bare = true;
                }
            }
            b"locked" => {
                if let Some(registration) = current.as_mut() {
                    registration.locked = Some(String::from_utf8_lossy(value).into_owned());
                }
            }
            b"prunable" => {
                if let Some(registration) = current.as_mut() {
                    registration.prunable = Some(String::from_utf8_lossy(value).into_owned());
                }
            }
            // Unknown keys are ignored so newer Git output stays readable.
            _ => {}
        }
    }
    if let Some(registration) = current.take() {
        registrations.push(registration);
    }
    if let Some(first) = registrations.first_mut() {
        first.is_main = true;
    }
    if registrations.is_empty() {
        return Err(GitError::new(
            GitErrorCode::InvalidOutput,
            "git worktree list printed no entries",
        ));
    }
    Ok(registrations)
}

/// Status facts plus submodule state parsed from one porcelain v2 run.
struct StatusParse {
    /// Fingerprint and count facts.
    facts: StatusFacts,
    /// Submodule dirtiness facts.
    submodules: SubmoduleFacts,
}

/// Parses `git status --porcelain=v2 -z --ignored=matching` bytes.
fn parse_status(bytes: &[u8]) -> StatusParse {
    let mut facts = StatusFacts {
        staged: 0,
        unstaged: 0,
        untracked: 0,
        conflicts: 0,
        ignored: Vec::new(),
        digest: hex_digest(bytes),
    };
    let mut submodules = SubmoduleFacts {
        dirty: false,
        unsupported: false,
    };
    for token in bytes.split(|&byte| byte == 0) {
        if token.is_empty() || token[0] == b'#' {
            continue;
        }
        let sub = token.get(5..9).filter(|field| field.len() == 4);
        match token[0] {
            // `1 <XY> <sub> ...` and `2 <XY> <sub> ... <path>[\t<orig>]`
            b'1' | b'2' => {
                if token.len() >= 4 {
                    if token[2] != b'.' {
                        facts.staged += 1;
                    }
                    if token[3] != b'.' {
                        facts.unstaged += 1;
                    }
                }
                if let Some(field) = sub {
                    if field[0] == b'S' {
                        if field[1..4].iter().any(|&flag| flag != b'.') {
                            submodules.dirty = true;
                        }
                    } else if field[0] != b'N' {
                        submodules.unsupported = true;
                    }
                }
            }
            // `u <XY> <sub> ...` unmerged entries
            b'u' => {
                facts.conflicts += 1;
                if sub.is_some_and(|field| field[0] == b'S') {
                    submodules.dirty = true;
                    submodules.unsupported = true;
                }
            }
            b'?' => facts.untracked += 1,
            b'!' if token.len() > 2 => {
                facts
                    .ignored
                    .push(String::from_utf8_lossy(&token[2..]).into_owned());
            }
            // Unknown record kinds are ignored for forward compatibility.
            _ => {}
        }
    }
    StatusParse { facts, submodules }
}

/// Lowercase SHA-256 hex digest of `bytes`.
fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Maps each admin directory under `worktrees/` to its recorded working tree.
///
/// Each admin entry's `gitdir` file holds the working tree path, which is the
/// only truthful way back from a prune output name to a worktree path.
fn admin_map(common_dir: &Path) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) else {
        return map;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(recorded) = std::fs::read_to_string(entry.path().join("gitdir")) {
            // The admin `gitdir` file names the worktree's `.git` entry (a
            // file for linked worktrees); the working tree is its parent.
            let git_entry = PathBuf::from(recorded.trim());
            let worktree = match git_entry.parent() {
                Some(parent)
                    if git_entry
                        .file_name()
                        .is_some_and(|n| n == OsStr::new(".git")) =>
                {
                    parent.to_path_buf()
                }
                _ => git_entry,
            };
            map.insert(name, worktree);
        }
    }
    map
}

/// Resolves a prune admin-directory name to its working tree path.
///
/// A matching registration confirms the recorded path; without one the recorded
/// path is still a real working tree, while an unreadable admin entry yields
/// `None`. An admin directory is never reported as a worktree path.
fn resolve_worktree_path(
    admins: &HashMap<String, PathBuf>,
    registrations: &[Registration],
    name: &str,
) -> Option<PathBuf> {
    let recorded = admins.get(name)?;
    Some(
        registrations
            .iter()
            .find(|registration| same_tree(&registration.path, recorded, recorded))
            .map_or(recorded.clone(), |registration| registration.path.clone()),
    )
}

/// Allocated bytes on disk for one metadata record.
fn allocated(meta: &std::fs::Metadata) -> u64 {
    meta.blocks() * 512
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::await_holding_lock,
    reason = "Test assertions, plus a deliberately held guard that serializes env-editing tests across awaits"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    /// Runs a synchronous Git call for fixture setup only.
    fn sync_git(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .unwrap()
    }

    /// Creates a temp repository on `main` with one commit and a `.gitignore`.
    fn repo_with_commit() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        sync_git(&main, &["init", "-q", "-b", "main"]);
        sync_git(&main, &["config", "user.email", "t@example.com"]);
        sync_git(&main, &["config", "user.name", "T"]);
        std::fs::write(main.join("a.txt"), "a\n").unwrap();
        std::fs::write(main.join(".gitignore"), "ign/\n").unwrap();
        sync_git(&main, &["add", "."]);
        sync_git(&main, &["commit", "-qm", "one"]);
        (dir, main)
    }

    /// Budget with generous caps for local fixture operations.
    fn budget_secs(secs: u64) -> Budget {
        Budget {
            deadline: Instant::now() + Duration::from_secs(secs),
            max_output_bytes: 1 << 20,
            max_entries: 100_000,
        }
    }

    /// Canonical path of a fixture directory, as Git records it.
    fn canon(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap()
    }

    /// Serializes tests while one of them edits process environment variables.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Acquires the environment lock for the whole test body.
    fn env_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Adds a linked worktree with a branch named after the directory.
    fn add_worktree(main: &Path, dir: &Path, extra: &[&str]) {
        let mut args: Vec<String> = vec!["worktree".into(), "add".into(), "-q".into()];
        for flag in extra {
            args.push((*flag).to_owned());
        }
        args.push(dir.to_string_lossy().into_owned());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = sync_git(main, &refs);
        assert!(
            out.status.success(),
            "fixture worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn worktree_list_parser_handles_raw_bytes() {
        let bytes = b"worktree /r\0HEAD 0000000000000000000000000000000000000000\0bare\0\0\
                      worktree /w\0HEAD abc\0detached\0locked\0\0";
        let registrations = parse_worktree_list(bytes).unwrap();
        assert_eq!(registrations.len(), 2);
        assert!(registrations[0].is_main && registrations[0].bare);
        assert_eq!(registrations[0].head, None);
        assert!(registrations[1].detached);
        assert_eq!(registrations[1].locked.as_deref(), Some(""));
        assert_eq!(registrations[1].head.as_deref(), Some("abc"));
        assert!(parse_worktree_list(b"\0").is_err());
    }

    #[test]
    fn status_parser_counts_and_flags() {
        let bytes = b"# branch.oid abc\0# branch.head main\0\
                      1 .M N... 100644 100644 100644 h h a\0\
                      1 MM S.CM 160000 160000 160000 h h sub\0\
                      u AA N... 100644 100644 100644 h h h m\0\
                      ? u.txt\0! ign/\0";
        let parsed = parse_status(bytes);
        assert_eq!((parsed.facts.staged, parsed.facts.unstaged), (1, 2));
        assert_eq!(parsed.facts.untracked, 1);
        assert_eq!(parsed.facts.conflicts, 1);
        assert_eq!(parsed.facts.ignored, vec!["ign/".to_owned()]);
        assert_eq!(parsed.facts.digest.len(), 64);
        assert!(parsed.submodules.dirty);
        assert!(!parsed.submodules.unsupported);
    }

    #[test]
    fn reflog_parser_reads_last_entry_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs").join("HEAD");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(
            &log,
            b"0000 1111 t <t@t> 1700000000 +0100\tclone: from x\n\
              2222 3333 t <t@t> 1790615966 +0200\tcheckout: moving to y\n",
        )
        .unwrap();
        assert_eq!(last_reflog_secs(&log), Some(1_790_615_966));
        std::fs::write(&log, b"garbage without fields").unwrap();
        assert_eq!(last_reflog_secs(&log), None);
        assert_eq!(last_reflog_secs(&dir.path().join("absent")), None);
    }

    #[tokio::test]
    async fn common_dir_is_shared_across_worktrees() {
        let _env = env_guard();
        let (dir, main) = repo_with_commit();
        let wt = dir.path().join("wt");
        add_worktree(&main, &wt, &[]);
        let cd_main = common_dir(&main, &budget_secs(30)).await.unwrap();
        let cd_wt = common_dir(&wt, &budget_secs(30)).await.unwrap();
        assert_eq!(cd_main, cd_wt);
        assert!(cd_main.ends_with(".git"));
        let error = common_dir(dir.path(), &budget_secs(30)).await.unwrap_err();
        assert_eq!(error.code, GitErrorCode::NotARepository);
    }

    #[tokio::test]
    async fn inventory_reports_registration_facts() {
        let _env = env_guard();
        let (dir, main) = repo_with_commit();
        let wt = dir.path().join("wt");
        let detached = dir.path().join("det");
        let gone = dir.path().join("gone");
        add_worktree(&main, &wt, &[]);
        add_worktree(&main, &detached, &["--detach"]);
        add_worktree(&main, &gone, &["--detach"]);
        sync_git(
            &main,
            &[
                "worktree",
                "lock",
                "--reason",
                "testing",
                &wt.to_string_lossy(),
            ],
        );
        let gone_canon = canon(&gone);
        std::fs::remove_dir_all(&gone).unwrap();
        let cd = common_dir(&main, &budget_secs(30)).await.unwrap();
        let registrations = inventory(&cd, &budget_secs(30)).await.unwrap();
        assert!(registrations.first().is_some_and(|r| r.is_main && !r.bare));
        let locked = registrations.iter().find(|r| r.path == canon(&wt)).unwrap();
        assert_eq!(locked.locked.as_deref(), Some("testing"));
        assert_eq!(locked.branch.as_deref(), Some("refs/heads/wt"));
        assert!(locked.prunable.is_none());
        let detached_reg = registrations
            .iter()
            .find(|r| r.path == canon(&detached))
            .unwrap();
        assert!(detached_reg.detached);
        assert_eq!(detached_reg.branch, None);
        assert!(detached_reg.head.is_some());
        let prunable = registrations.iter().find(|r| r.path == gone_canon).unwrap();
        assert!(prunable.prunable.is_some());
    }

    #[tokio::test]
    async fn observe_reports_status_signals_and_integration() {
        let _env = env_guard();
        let (dir, main) = repo_with_commit();
        let wt = dir.path().join("wt");
        add_worktree(&main, &wt, &[]);
        let cd = common_dir(&main, &budget_secs(30)).await.unwrap();
        let all = Checks {
            status: true,
            integration: true,
            submodules: true,
            ..Default::default()
        };
        let clean = observe(
            &cd,
            &ObserveSpec {
                worktree_path: &wt,
                integration_ref: Some("main"),
                checks: all,
            },
            &budget_secs(60),
        )
        .await
        .unwrap();
        let Probe::Known(facts) = &clean.status else {
            panic!("clean status probe not known: {:?}", clean.status);
        };
        assert_eq!(
            (
                facts.staged,
                facts.unstaged,
                facts.untracked,
                facts.conflicts
            ),
            (0, 0, 0, 0)
        );
        assert!(facts.ignored.is_empty());
        assert!(matches!(
            clean.integration,
            Probe::Known(Integration::AncestorMerged)
        ));
        assert!(matches!(clean.live_processes, Probe::NotChecked));
        assert!(clean.activity_signals.head_mtime.is_some());
        assert!(clean.activity_signals.index_mtime.is_some());
        assert!(clean.activity_signals.last_reflog_entry.is_some());

        std::fs::write(wt.join("a.txt"), "changed\n").unwrap();
        std::fs::write(wt.join("u.txt"), "u\n").unwrap();
        std::fs::create_dir_all(wt.join("ign")).unwrap();
        std::fs::write(wt.join("ign/x.log"), "x\n").unwrap();
        let dirty = observe(
            &cd,
            &ObserveSpec {
                worktree_path: &wt,
                integration_ref: Some("main"),
                checks: all,
            },
            &budget_secs(60),
        )
        .await
        .unwrap();
        let Probe::Known(dirty_facts) = &dirty.status else {
            panic!("dirty status probe not known");
        };
        assert_eq!(dirty_facts.unstaged, 1);
        assert_eq!(dirty_facts.untracked, 1);
        assert_eq!(dirty_facts.ignored, vec!["ign/".to_owned()]);
        assert_ne!(dirty_facts.digest, facts.digest);

        // A commit that main does not contain flips ancestry to unmerged.
        sync_git(&wt, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let unmerged = observe(
            &cd,
            &ObserveSpec {
                worktree_path: &wt,
                integration_ref: Some("main"),
                checks: Checks {
                    integration: true,
                    ..Default::default()
                },
            },
            &budget_secs(60),
        )
        .await
        .unwrap();
        assert!(matches!(
            unmerged.integration,
            Probe::Known(Integration::Unmerged)
        ));

        // A missing integration ref leaves ancestry unknown, not unavailable.
        let missing = observe(
            &cd,
            &ObserveSpec {
                worktree_path: &wt,
                integration_ref: Some("no-such-ref"),
                checks: Checks {
                    integration: true,
                    ..Default::default()
                },
            },
            &budget_secs(60),
        )
        .await
        .unwrap();
        assert!(matches!(
            missing.integration,
            Probe::Known(Integration::Unknown)
        ));
    }

    #[tokio::test]
    async fn create_adds_worktree_suppressing_hooks_and_refuses_conflicts() {
        let _env = env_guard();
        let (dir, main) = repo_with_commit();
        let hook = main.join(".git/hooks/post-checkout");
        std::fs::write(&hook, "#!/bin/sh\necho ran > \"$PWD/hook-ran.txt\"\n").unwrap();
        let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).unwrap();
        let cd = common_dir(&main, &budget_secs(30)).await.unwrap();
        let name = WorktreeName::parse("task-1").unwrap();

        // Parent directories are created; hooks never run.
        let dest = dir.path().join("root/task-1");
        let created = create(
            &cd,
            &CreateSpec {
                name: &name,
                destination: &dest,
                base: None,
                branch: None,
                detached: false,
            },
            &budget_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(created.branch.as_deref(), Some("aw/task-1"));
        assert_eq!(created.path, canon(&dest));
        assert_eq!(created.head.len(), 40);
        assert!(dest.join("a.txt").exists());
        assert!(!dest.join("hook-ran.txt").exists());

        // An existing destination is a conflict, never an overwrite.
        let error = create(
            &cd,
            &CreateSpec {
                name: &name,
                destination: &dest,
                base: None,
                branch: None,
                detached: false,
            },
            &budget_secs(60),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, GitErrorCode::Conflict);

        // The default branch now exists, so the same name cannot be re-created.
        let error = create(
            &cd,
            &CreateSpec {
                name: &name,
                destination: &dir.path().join("root/task-1b"),
                base: None,
                branch: None,
                detached: false,
            },
            &budget_secs(60),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, GitErrorCode::Conflict);

        // An explicit existing branch is checked out, not reset.
        sync_git(&main, &["branch", "feat"]);
        let branch_dest = dir.path().join("root/featwt");
        let created = create(
            &cd,
            &CreateSpec {
                name: &name,
                destination: &branch_dest,
                base: None,
                branch: Some("feat"),
                detached: false,
            },
            &budget_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(created.branch.as_deref(), Some("feat"));

        // base plus an existing branch is refused instead of dropping base.
        let error = create(
            &cd,
            &CreateSpec {
                name: &name,
                destination: &dir.path().join("root/bb"),
                base: Some("main"),
                branch: Some("feat"),
                detached: false,
            },
            &budget_secs(60),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, GitErrorCode::Conflict);

        // Detached creation has no branch but a concrete HEAD.
        let detached_dest = dir.path().join("root/detwt");
        let created = create(
            &cd,
            &CreateSpec {
                name: &name,
                destination: &detached_dest,
                base: Some("main"),
                branch: None,
                detached: true,
            },
            &budget_secs(60),
        )
        .await
        .unwrap();
        assert_eq!(created.branch, None);
        assert_eq!(created.head.len(), 40);

        // A branch checked out in another worktree cannot be reused.
        let error = create(
            &cd,
            &CreateSpec {
                name: &name,
                destination: &dir.path().join("root/again"),
                base: None,
                branch: Some("feat"),
                detached: false,
            },
            &budget_secs(60),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, GitErrorCode::Conflict);
        // Unknown base refs are reported as not found.
        let error = create(
            &cd,
            &CreateSpec {
                name: &name,
                destination: &dir.path().join("root/badbase"),
                base: Some("no-such"),
                branch: None,
                detached: true,
            },
            &budget_secs(60),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, GitErrorCode::NotFound);
    }

    #[tokio::test]
    async fn remove_refuses_dirty_then_succeeds_and_replays_absent() {
        let _env = env_guard();
        let (dir, main) = repo_with_commit();
        let wt = dir.path().join("wt");
        add_worktree(&main, &wt, &[]);
        let cd = common_dir(&main, &budget_secs(30)).await.unwrap();

        std::fs::write(wt.join("u.txt"), "u\n").unwrap();
        let error = remove(&cd, &wt, &budget_secs(60)).await.unwrap_err();
        assert_eq!(error.code, GitErrorCode::Conflict);
        assert!(
            error.detail.contains("untracked"),
            "git refusal not carried: {error}"
        );

        std::fs::remove_file(wt.join("u.txt")).unwrap();
        assert_eq!(
            remove(&cd, &wt, &budget_secs(60)).await.unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!wt.exists());
        // The branch survives removal; commits are never lost.
        assert!(
            sync_git(&main, &["rev-parse", "--verify", "refs/heads/wt"])
                .status
                .success()
        );
        assert_eq!(
            remove(&cd, &wt, &budget_secs(60)).await.unwrap(),
            RemoveOutcome::AlreadyAbsent
        );
    }

    #[tokio::test]
    async fn prune_dry_run_lists_candidates_and_apply_removes_them() {
        let _env = env_guard();
        let (dir, main) = repo_with_commit();
        let gone = dir.path().join("gone");
        add_worktree(&main, &gone, &["--detach"]);
        let gone_canon = canon(&gone);
        std::fs::remove_dir_all(&gone).unwrap();
        let cd = common_dir(&main, &budget_secs(30)).await.unwrap();

        let dry = prune(&cd, true, &budget_secs(30)).await.unwrap();
        assert!(!dry.applied);
        assert_eq!(dry.candidates, vec![gone_canon.clone()]);
        assert!(
            inventory(&cd, &budget_secs(30))
                .await
                .unwrap()
                .iter()
                .any(|r| r.path == gone_canon)
        );

        let applied = prune(&cd, false, &budget_secs(30)).await.unwrap();
        assert!(applied.applied);
        assert_eq!(applied.candidates.len(), 1);
        assert!(
            inventory(&cd, &budget_secs(30))
                .await
                .unwrap()
                .iter()
                .all(|r| r.path != gone_canon)
        );
        let again = prune(&cd, true, &budget_secs(30)).await.unwrap();
        assert!(again.candidates.is_empty());
    }

    #[tokio::test]
    async fn expired_budget_times_out_and_output_cap_fails_loudly() {
        let _env = env_guard();
        let (dir, main) = repo_with_commit();
        let expired = Budget {
            deadline: Instant::now() - Duration::from_secs(1),
            max_output_bytes: 1 << 20,
            max_entries: 10,
        };
        assert_eq!(
            common_dir(&main, &expired).await.unwrap_err().code,
            GitErrorCode::Timeout
        );
        let cd = common_dir(&main, &budget_secs(30)).await.unwrap();
        assert_eq!(
            inventory(&cd, &expired).await.unwrap_err().code,
            GitErrorCode::Timeout
        );
        // A cap that cannot hold the answer is an error, never a truncation.
        let tiny = Budget {
            deadline: Instant::now() + Duration::from_secs(30),
            max_output_bytes: 8,
            max_entries: 10,
        };
        assert_eq!(
            common_dir(&main, &tiny).await.unwrap_err().code,
            GitErrorCode::InvalidOutput
        );
        // The detached worktree destination never existed.
        assert!(!dir.path().join("never").exists());
    }

    #[tokio::test]
    async fn size_is_lower_bound_under_tight_entry_budget() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("tree");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for index in 0..8 {
            std::fs::write(root.join(format!("f{index}.txt")), "0123456789\n").unwrap();
        }
        let full = measure_size(&root, &budget_secs(30)).await.unwrap();
        assert_eq!(full.quality, SizeQuality::Complete);
        assert!(full.bytes > 0);
        let tight = Budget {
            deadline: Instant::now() + Duration::from_secs(30),
            max_output_bytes: 1 << 20,
            max_entries: 3,
        };
        let partial = measure_size(&root, &tight).await.unwrap();
        assert_eq!(partial.quality, SizeQuality::LowerBound);
        assert!(partial.bytes < full.bytes);
        let error = measure_size(&dir.path().join("missing"), &budget_secs(30))
            .await
            .unwrap_err();
        assert_eq!(error.code, GitErrorCode::NotFound);
    }

    #[tokio::test]
    async fn live_process_with_cwd_inside_is_detected() {
        let _env = env_guard();
        let (dir, main) = repo_with_commit();
        let wt = dir.path().join("wt");
        add_worktree(&main, &wt, &[]);
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&wt)
            .spawn()
            .unwrap();
        let found = live_processes_under(&wt, &budget_secs(60)).await.unwrap();
        assert!(
            found.iter().any(|process| process.pid == child.id()),
            "spawned process not found in {found:?}"
        );
        let outside = live_processes_under(&dir.path().join("absent"), &budget_secs(60)).await;
        assert!(outside.is_err() || outside.unwrap().is_empty());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[tokio::test]
    async fn inherited_environment_cannot_rebind_the_repository() {
        let _env = env_guard();
        let (_dir_a, main_a) = repo_with_commit();
        let (_dir_b, main_b) = repo_with_commit();
        // As if the server process itself had been started inside repo A.
        // SAFETY: every Git-spawning test holds ENV_LOCK, so no other thread
        // reads or writes the environment while the variables are set.
        unsafe {
            std::env::set_var("GIT_DIR", main_a.join(".git"));
            std::env::set_var("GIT_WORK_TREE", &main_a);
        }
        let resolved = common_dir(&main_b, &budget_secs(30)).await;
        // SAFETY: see above.
        unsafe {
            std::env::remove_var("GIT_DIR");
            std::env::remove_var("GIT_WORK_TREE");
        }
        assert_eq!(
            resolved.unwrap(),
            std::fs::canonicalize(main_b.join(".git")).unwrap()
        );
    }

    #[tokio::test]
    async fn ignored_only_worktree_is_reported_and_removable() {
        let _env = env_guard();
        let (dir, main) = repo_with_commit();
        let wt = dir.path().join("wt");
        add_worktree(&main, &wt, &[]);
        std::fs::create_dir_all(wt.join("ign")).unwrap();
        std::fs::write(wt.join("ign/x.log"), "x\n").unwrap();
        let cd = common_dir(&main, &budget_secs(30)).await.unwrap();
        let observed = observe(
            &cd,
            &ObserveSpec {
                worktree_path: &wt,
                integration_ref: None,
                checks: Checks {
                    status: true,
                    ..Default::default()
                },
            },
            &budget_secs(60),
        )
        .await
        .unwrap();
        let Probe::Known(facts) = &observed.status else {
            panic!("status probe not known: {:?}", observed.status);
        };
        assert_eq!(facts.ignored, vec!["ign/".to_owned()]);
        assert_eq!(
            (
                facts.staged,
                facts.unstaged,
                facts.untracked,
                facts.conflicts
            ),
            (0, 0, 0, 0)
        );
        // Git removes ignored-only worktrees without --force; the caller-side
        // IgnoredNotDisposable veto is the only guard, as documented on remove.
        assert_eq!(
            remove(&cd, &wt, &budget_secs(60)).await.unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!wt.exists());
    }
}
