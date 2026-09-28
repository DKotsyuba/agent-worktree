//! Shared worktree contracts: identities, observations, state records and pure policy.
//!
//! This module owns the data shapes exchanged between Git access (`crate::git`),
//! persistent state (`crate::store`) and the MCP tools. Everything here is pure:
//! no filesystem, subprocess or clock access. `classify` and `assess_removal`
//! answer conservatively: an unknown probe is never treated as a clean result.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Versioned encoding tag for repository identity hashing.
const REPO_ID_VERSION: &str = "aw-repo-id-v1";
/// Versioned encoding tag for removal fingerprints.
const FINGERPRINT_VERSION: &str = "aw-fingerprint-v1";
/// Current on-disk record schema version.
pub const RECORD_SCHEMA_VERSION: u32 = 1;

/// Repository identity: SHA-256 of the canonical absolute Git common directory.
///
/// Linked worktrees share one identity; independent clones remain distinct.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct RepoId(String);

impl RepoId {
    /// Computes the identity from a canonical absolute common-directory path.
    #[must_use]
    pub fn from_common_dir(common_dir: &Path) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(REPO_ID_VERSION.as_bytes());
        hasher.update([0]);
        hasher.update(common_dir.as_os_str().as_encoded_bytes());
        Self(hex(&hasher.finalize()))
    }

    /// Returns the full 64-character lowercase hex identity.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the first 12 characters used in directory names and tool ids.
    #[must_use]
    pub fn id12(&self) -> &str {
        &self.0[..12]
    }

    /// Parses an existing identity; rejects anything but 64 lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, ValidationError> {
        if value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(ValidationError::IdentityFormat)
        }
    }
}

/// Caller-supplied worktree name: 1–64 characters of `[a-z0-9-]`, no leading hyphen.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct WorktreeName(String);

impl WorktreeName {
    /// Validates and adopts a worktree name.
    pub fn parse(value: &str) -> Result<Self, ValidationError> {
        if !(1..=64).contains(&value.len()) {
            return Err(ValidationError::NameLength);
        }
        let charset_ok = value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !value.starts_with('-');
        if charset_ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(ValidationError::NameCharset)
        }
    }

    /// Returns the validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the default branch name `aw/<name>`.
    #[must_use]
    pub fn default_branch(&self) -> String {
        format!("aw/{}", self.0)
    }
}

/// Builds the per-repository directory name `<label>--<id12>` under the worktree root.
///
/// The label is display-only; identity never depends on it.
#[must_use]
pub fn repo_directory(label: &str, repo_id: &RepoId) -> String {
    format!("{label}--{}", repo_id.id12())
}

/// Validates a repository label: 1–32 ASCII `[a-z0-9.-]`, no leading hyphen or dot.
pub fn validate_label(label: &str) -> Result<(), ValidationError> {
    if !(1..=32).contains(&label.len()) {
        return Err(ValidationError::LabelLength);
    }
    let charset_ok = label
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
        && !label.starts_with(['-', '.']);
    if charset_ok {
        Ok(())
    } else {
        Err(ValidationError::LabelCharset)
    }
}

/// Validates a branch short name against a safe subset of Git refname rules.
///
/// This is a conservative allowlist, not a full refname parser; exotic but legal
/// refnames are rejected rather than misparsed.
pub fn validate_branch(branch: &str) -> Result<(), ValidationError> {
    let shape_ok = !branch.is_empty()
        && branch.len() <= 200
        && !branch.starts_with(['-', '.', '/'])
        && !branch.ends_with(['/', '.', ' ', '\t'])
        && !branch.ends_with(".lock")
        && !branch.contains("..")
        && !branch.contains("//")
        && !branch
            .bytes()
            .any(|b| b <= 0x20 || b == 0x7f || b" ~^:?*[]\\".contains(&b));
    if shape_ok {
        Ok(())
    } else {
        Err(ValidationError::BranchUnsafe)
    }
}

/// Stable identity of one worktree: owning repository plus directory name.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct WorktreeId {
    /// Owning repository identity.
    pub repo: RepoId,
    /// Worktree directory base name; not restricted to our alphabet for foreign worktrees.
    pub name: String,
}

impl WorktreeId {
    /// Returns the compact tool-facing key `<id12>/<name>`.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}/{}", self.repo.id12(), self.name)
    }
}

/// One entry of `git worktree list --porcelain`: Git's own registration facts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Registration {
    /// Absolute path of the working tree (or repository root when bare).
    pub path: PathBuf,
    /// HEAD commit id, when Git reported it.
    pub head: Option<String>,
    /// Full branch ref, when HEAD is attached.
    pub branch: Option<String>,
    /// True when HEAD is detached.
    pub detached: bool,
    /// True for bare registrations.
    pub bare: bool,
    /// Present when the worktree is locked via `git worktree lock`.
    pub locked: Option<String>,
    /// Prunable reason reported by Git, when present.
    pub prunable: Option<String>,
    /// True for the main worktree entry.
    pub is_main: bool,
}

/// Ownership classification of a worktree path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorktreeClass {
    /// Created through this product and backed by a record.
    Managed,
    /// Registered with Git without product metadata.
    Foreign,
    /// Registered path absent on disk.
    Missing,
    /// Directory present without an accessible Git registration; never auto-deletable.
    OrphanCandidate,
}

/// Outcome of one observation axis; unknown is never mistaken for a clean result.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Probe<T> {
    /// The check ran and produced a value.
    Known(T),
    /// The check was not requested.
    NotChecked,
    /// The check could not run at all; carries a stable reason code.
    Unavailable {
        /// Stable reason code identifying the failure class.
        code: String,
    },
    /// The check ran partially; `evidence` holds the guaranteed part.
    Incomplete {
        /// The part of the result that is guaranteed correct.
        evidence: T,
        /// Why the rest of the check is missing.
        reason: String,
    },
}

/// Activity axis derived from freshness signals.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Activity {
    /// A live process has its cwd inside the tree.
    Active,
    /// Newest signal is within `Policy::recent_after_secs`.
    Recent,
    /// Newest signal is at least `Policy::idle_after_secs` old.
    IdleCandidate,
    /// Newest signal is at least `Policy::stale_after_secs` old.
    StaleCandidate,
    /// No usable signal; never treated as proof of abandonment.
    Unknown,
}

/// Integration axis: ancestry of HEAD under the configured integration ref only.
///
/// Squash merges are not detected; a squash-merged branch still reports unmerged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Integration {
    /// HEAD is an ancestor of the integration ref.
    AncestorMerged,
    /// HEAD has commits not contained in the integration ref.
    Unmerged,
    /// Ancestry could not be determined.
    Unknown,
}

/// Quality of a size measurement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SizeQuality {
    /// The whole tree was measured.
    Complete,
    /// Traversal was budget-cut; the value is a lower bound.
    LowerBound,
}

/// On-disk size of one worktree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Size {
    /// Measured or lower-bound byte count.
    pub bytes: u64,
    /// Whether the measurement completed.
    pub quality: SizeQuality,
}

/// Facts from one `git status --porcelain=v2 -z --ignored=matching` run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StatusFacts {
    /// Number of staged tracked changes.
    pub staged: u64,
    /// Number of unstaged tracked changes.
    pub unstaged: u64,
    /// Number of untracked files.
    pub untracked: u64,
    /// Number of unresolved conflict entries.
    pub conflicts: u64,
    /// Ignored entries as printed by Git.
    pub ignored: Vec<String>,
    /// Hex digest of the raw status output; part of the removal fingerprint.
    pub digest: String,
}

/// Submodule observation used by removal vetoes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SubmoduleFacts {
    /// True when any submodule has a dirty working tree.
    pub dirty: bool,
    /// True when submodule state is present but not representable here.
    pub unsupported: bool,
}

/// Cheap activity timestamps (unix seconds); absent entries stay `None`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ActivitySignals {
    /// mtime of the HEAD file.
    pub head_mtime: Option<u64>,
    /// mtime of the index file.
    pub index_mtime: Option<u64>,
    /// Timestamp inside the latest HEAD reflog entry (not the file's mtime).
    pub last_reflog_entry: Option<u64>,
}

/// All evidence about one worktree at one point in time.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Observation {
    /// Identity of the observed worktree.
    pub id: WorktreeId,
    /// Unix seconds when the observation batch finished.
    pub observed_at: u64,
    /// Git registration facts for the worktree.
    pub registration: Registration,
    /// Cheap activity timestamps.
    pub activity_signals: ActivitySignals,
    /// Whether a live process has its cwd inside the tree.
    pub live_processes: Probe<bool>,
    /// Status facts including the fingerprint digest.
    pub status: Probe<StatusFacts>,
    /// Submodule dirtiness.
    pub submodules: Probe<SubmoduleFacts>,
    /// Ancestry under the configured integration ref.
    pub integration: Probe<Integration>,
    /// On-disk size, when requested.
    pub size: Probe<Size>,
}

/// Durable per-worktree record stored under `<home>/state/v1/repos/<repo-id>/`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// On-disk schema version; currently `RECORD_SCHEMA_VERSION`.
    pub schema_version: u32,
    /// Owning repository identity (64 hex characters).
    pub repo_id: String,
    /// Worktree directory base name.
    pub name: String,
    /// Bound absolute worktree path at creation time.
    pub path: PathBuf,
    /// Branch checked out; default `aw/<name>`.
    pub branch: Option<String>,
    /// Base ref requested at creation, when recorded.
    pub base_ref: Option<String>,
    /// Resolved base commit at creation, when recorded.
    pub base_oid: Option<String>,
    /// Creation time (unix seconds).
    pub created_at: u64,
    /// Creator harness attribution; attribution, not authentication.
    pub creator: String,
    /// Optimistic concurrency counter; bumped on every successful write.
    pub revision: u64,
    /// Written before a removal is dispatched, so interrupts stay visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removal_started: Option<RemovalStarted>,
}

/// Marker written to a record before the removal is dispatched.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovalStarted {
    /// Fingerprint accepted at apply time.
    pub fingerprint: Fingerprint,
    /// Unix seconds when the removal was dispatched.
    pub at: u64,
}

/// SHA-256 removal fingerprint binding a preview to its apply step.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Parses a fingerprint; rejects anything but 64 lowercase hex characters.
    pub fn parse(value: &str) -> Result<Self, ValidationError> {
        RepoId::parse(value).map(|_| Self(value.to_owned()))
    }

    /// Returns the 64-character lowercase hex fingerprint.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Inputs to `fingerprint`; every field is part of the hash.
#[derive(Clone, Copy)]
pub struct FingerprintInput<'a> {
    /// Canonical absolute worktree path.
    pub path: &'a Path,
    /// Current HEAD commit id.
    pub head: &'a str,
    /// Current branch ref.
    pub branch: &'a str,
    /// Digest of the raw `git status --porcelain=v2 -z --ignored=matching` output.
    pub status_digest: &'a str,
    /// Ignored paths approved for deletion, worktree-relative.
    pub disposable_paths: &'a [PathBuf],
    /// Record revision at preview time; 0 when no record exists.
    pub record_revision: u64,
    /// Policy revision at preview time.
    pub policy_revision: u64,
}

/// Computes the removal fingerprint over a versioned, length-prefixed encoding of all inputs.
///
/// `disposable_paths` are normalized (sorted, deduplicated) before hashing, so an
/// unordered but equivalent list produces the same fingerprint.
#[must_use]
pub fn fingerprint(input: &FingerprintInput<'_>) -> Fingerprint {
    let mut hasher = Sha256::new();
    hasher.update(FINGERPRINT_VERSION.as_bytes());
    hash_field(&mut hasher, input.path.as_os_str().as_encoded_bytes());
    hash_field(&mut hasher, input.head.as_bytes());
    hash_field(&mut hasher, input.branch.as_bytes());
    hash_field(&mut hasher, input.status_digest.as_bytes());
    let mut paths: Vec<&[u8]> = input
        .disposable_paths
        .iter()
        .map(|p| p.as_os_str().as_encoded_bytes())
        .collect();
    paths.sort_unstable();
    paths.dedup();
    hasher.update((paths.len() as u64).to_le_bytes());
    for path in paths {
        hash_field(&mut hasher, path);
    }
    hasher.update(input.record_revision.to_le_bytes());
    hasher.update(input.policy_revision.to_le_bytes());
    Fingerprint(hex(&hasher.finalize()))
}

/// Hashes one length-prefixed field so adjacent values cannot be confused.
fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Thresholds and revision for classification and vetoes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Policy {
    /// Bumped whenever thresholds change; part of the removal fingerprint.
    pub revision: u64,
    /// Signals fresher than this are `Activity::Recent`.
    pub recent_after_secs: u64,
    /// Signals at least this old are `Activity::IdleCandidate`.
    pub idle_after_secs: u64,
    /// Signals at least this old are `Activity::StaleCandidate`.
    pub stale_after_secs: u64,
    /// Size at or above this many bytes produces a size warning.
    pub size_warning_bytes: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            revision: 1,
            recent_after_secs: 24 * 60 * 60,
            idle_after_secs: 7 * 24 * 60 * 60,
            stale_after_secs: 30 * 24 * 60 * 60,
            size_warning_bytes: 2_u64 << 30,
        }
    }
}

/// Non-blocking advice derived from an observation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Advice {
    /// Classified activity axis.
    pub activity: Activity,
    /// Warnings worth surfacing in list and inspect replies.
    pub warnings: Vec<Warning>,
}

/// Advisory (non-blocking) finding attached to `Advice` or `Decision`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Warning {
    /// Measured size is at least this many bytes.
    SizeAtLeast {
        /// Lower-bound or complete byte count.
        bytes: u64,
    },
    /// HEAD is not merged into the integration ref.
    Unmerged,
    /// Integration could not be determined.
    IntegrationUnknown,
    /// A probe ran partially; the reason names the missing part.
    ProbeIncomplete {
        /// Why part of the check is missing.
        reason: String,
    },
    /// The record shows a dispatched removal that never completed.
    RemovalStarted,
}

/// A removal request: preview parameters plus the apply-time expectation.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct RemovalRequest {
    /// Apply only: fingerprint returned by the preview being applied.
    pub expected_fingerprint: Option<Fingerprint>,
    /// Ignored paths approved for deletion, worktree-relative.
    pub disposable_paths: Vec<PathBuf>,
    /// Explicit confirmation allowing removal of an unmerged worktree; the branch is retained.
    pub allow_unmerged: bool,
}

/// Blocking result of removal assessment.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Decision {
    /// Reasons removal is refused; empty means removal may proceed.
    pub vetoes: Vec<Veto>,
    /// Non-blocking findings to surface next to the refusal or receipt.
    pub warnings: Vec<Warning>,
    /// Current fingerprint, present when the required probes were known.
    pub fingerprint: Option<Fingerprint>,
}

/// One blocking reason removal is refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Veto {
    /// The decision logic is not implemented yet; removal is always refused.
    NotImplemented,
    /// The target is the main worktree.
    MainWorktree,
    /// The target is a bare repository root.
    BareRepository,
    /// The worktree is locked via `git worktree lock`.
    GitLocked,
    /// Staged or unstaged tracked changes exist.
    Dirty,
    /// Untracked files exist.
    UntrackedFiles,
    /// Unresolved merge conflicts exist.
    Conflicts,
    /// Submodules are dirty or their state is unsupported.
    DirtySubmodules,
    /// Ignored files exist outside the approved disposable paths.
    IgnoredNotDisposable {
        /// The offending ignored paths.
        paths: Vec<String>,
    },
    /// A live process has its cwd inside the tree.
    LiveProcess,
    /// The apply-time fingerprint differs from the preview.
    FingerprintMismatch,
    /// A required probe was not checked, unavailable or incomplete.
    ProbeUnknown,
    /// HEAD is unmerged and `allow_unmerged` was not set.
    Unmerged,
}

impl Veto {
    /// Returns the stable snake_case code used in tool replies.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotImplemented => "not_implemented",
            Self::MainWorktree => "main_worktree",
            Self::BareRepository => "bare_repository",
            Self::GitLocked => "git_locked",
            Self::Dirty => "dirty",
            Self::UntrackedFiles => "untracked_files",
            Self::Conflicts => "conflicts",
            Self::DirtySubmodules => "dirty_submodules",
            Self::IgnoredNotDisposable { .. } => "ignored_not_disposable",
            Self::LiveProcess => "live_process",
            Self::FingerprintMismatch => "fingerprint_mismatch",
            Self::ProbeUnknown => "probe_unknown",
            Self::Unmerged => "unmerged",
        }
    }
}

/// Validation failure for caller-supplied names, labels, branches or identities.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ValidationError {
    /// Worktree name length outside 1–64.
    NameLength,
    /// Worktree name outside `[a-z0-9-]` or leading hyphen.
    NameCharset,
    /// Label length outside 1–32.
    LabelLength,
    /// Label outside `[a-z0-9.-]` or leading hyphen or dot.
    LabelCharset,
    /// Branch is not a safe refname.
    BranchUnsafe,
    /// Identity is not 64 lowercase hex characters.
    IdentityFormat,
}

impl ValidationError {
    /// Returns the stable snake_case code used in tool replies.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NameLength => "name_length",
            Self::NameCharset => "name_charset",
            Self::LabelLength => "label_length",
            Self::LabelCharset => "label_charset",
            Self::BranchUnsafe => "branch_unsafe",
            Self::IdentityFormat => "identity_format",
        }
    }
}

/// Explicit deadline and caps for one probe or mutation batch.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    /// Hard deadline for the whole operation, including lock waits.
    pub deadline: Instant,
    /// Cap on any single subprocess output.
    pub max_output_bytes: usize,
    /// Cap on directory-walk entries.
    pub max_entries: usize,
}

/// Classifies an observation into activity advice and warnings.
///
/// Ordering rules: a live process is `Active`; otherwise the newest available
/// signal decides — within `recent_after_secs` is `Recent`, at least
/// `idle_after_secs` is `IdleCandidate`, at least `stale_after_secs` is
/// `StaleCandidate`; between `recent` and `idle` the answer stays `Recent`
/// (claiming activity longer is the conservative direction). Missing signals
/// yield `Unknown`.
///
/// Warnings never block: size at or above the policy threshold (a lower bound
/// counts), unmerged or undeterminable integration, and one `ProbeIncomplete`
/// per probe that ran partially. The record is not an input here, so this
/// function cannot emit `Warning::RemovalStarted`; the caller holding the
/// record must add that warning itself when `removal_started` is set.
#[must_use]
pub fn classify(observation: &Observation, policy: &Policy, now: u64) -> Advice {
    let mut warnings = Vec::new();
    collect_warnings(observation, policy, &mut warnings);
    let activity = if probe_value(&observation.live_processes).copied() == Some(true) {
        Activity::Active
    } else {
        match newest_signal(observation) {
            None => Activity::Unknown,
            Some(newest) => {
                let age = now.saturating_sub(newest);
                if age >= policy.stale_after_secs {
                    Activity::StaleCandidate
                } else if age >= policy.idle_after_secs {
                    Activity::IdleCandidate
                } else {
                    Activity::Recent
                }
            }
        }
    };
    Advice { activity, warnings }
}

/// Assesses a removal request against the observation, record and policy.
///
/// Returns every blocking veto and every warning; empty vetoes mean removal
/// may proceed. An unknown, unavailable or incomplete required probe is never
/// treated as clean: it adds `Veto::ProbeUnknown`. The current fingerprint is
/// always computed from the best available status evidence and returned when
/// the status probe produced a digest; `expected_fingerprint` is compared
/// against it only when both exist.
#[must_use]
pub fn assess_removal(
    observation: &Observation,
    record: Option<&Record>,
    request: &RemovalRequest,
    policy: &Policy,
) -> Decision {
    let mut vetoes = Vec::new();
    let mut warnings = Vec::new();
    collect_warnings(observation, policy, &mut warnings);
    if record.is_some_and(|r| r.removal_started.is_some()) {
        warnings.push(Warning::RemovalStarted);
    }

    let registration = &observation.registration;
    if registration.is_main {
        vetoes.push(Veto::MainWorktree);
    }
    if registration.bare {
        vetoes.push(Veto::BareRepository);
    }
    if registration.locked.is_some() {
        vetoes.push(Veto::GitLocked);
    }

    if let Some(facts) = probe_value(&observation.status) {
        if facts.staged > 0 || facts.unstaged > 0 {
            vetoes.push(Veto::Dirty);
        }
        if facts.untracked > 0 {
            vetoes.push(Veto::UntrackedFiles);
        }
        if facts.conflicts > 0 {
            vetoes.push(Veto::Conflicts);
        }
        let not_disposable: Vec<String> = facts
            .ignored
            .iter()
            .filter(|path| !is_disposable(path, &request.disposable_paths))
            .cloned()
            .collect();
        if !not_disposable.is_empty() {
            vetoes.push(Veto::IgnoredNotDisposable {
                paths: not_disposable,
            });
        }
    }

    if let Some(submodules) = probe_value(&observation.submodules)
        && (submodules.dirty || submodules.unsupported)
    {
        vetoes.push(Veto::DirtySubmodules);
    }

    if probe_value(&observation.live_processes).copied() == Some(true) {
        vetoes.push(Veto::LiveProcess);
    }

    // Status, processes and submodules are always required; integration is
    // required to prove mergedness unless the request explicitly waives it.
    let mut probe_unknown = !is_known(&observation.status)
        || !is_known(&observation.live_processes)
        || !is_known(&observation.submodules);
    if !request.allow_unmerged {
        probe_unknown = probe_unknown || !is_known(&observation.integration);
    }
    if probe_unknown {
        vetoes.push(Veto::ProbeUnknown);
    }

    if !request.allow_unmerged
        && probe_value(&observation.integration).copied() == Some(Integration::Unmerged)
    {
        vetoes.push(Veto::Unmerged);
    }

    let current = probe_value(&observation.status).map(|facts| {
        fingerprint(&FingerprintInput {
            path: &registration.path,
            head: registration.head.as_deref().unwrap_or(""),
            branch: registration.branch.as_deref().unwrap_or(""),
            status_digest: &facts.digest,
            disposable_paths: &request.disposable_paths,
            record_revision: record.map_or(0, |r| r.revision),
            policy_revision: policy.revision,
        })
    });
    if let (Some(expected), Some(actual)) = (&request.expected_fingerprint, &current)
        && expected != actual
    {
        vetoes.push(Veto::FingerprintMismatch);
    }

    Decision {
        vetoes,
        warnings,
        fingerprint: current,
    }
}

/// Newest of the cheap activity timestamps; `None` when every signal is absent.
fn newest_signal(observation: &Observation) -> Option<u64> {
    let signals = &observation.activity_signals;
    [
        signals.head_mtime,
        signals.index_mtime,
        signals.last_reflog_entry,
    ]
    .into_iter()
    .flatten()
    .max()
}

/// Guaranteed value of a probe: the payload of `Known` or the evidence of
/// `Incomplete`; `None` for `NotChecked` and `Unavailable`.
fn probe_value<T>(probe: &Probe<T>) -> Option<&T> {
    match probe {
        Probe::Known(value)
        | Probe::Incomplete {
            evidence: value, ..
        } => Some(value),
        Probe::NotChecked | Probe::Unavailable { .. } => None,
    }
}

/// Reason text when a probe ran partially; `None` when it did not.
fn incomplete_reason<T>(probe: &Probe<T>) -> Option<&str> {
    match probe {
        Probe::Incomplete { reason, .. } => Some(reason),
        Probe::Known(_) | Probe::NotChecked | Probe::Unavailable { .. } => None,
    }
}

/// True only when the probe completed fully; partial evidence is not certainty.
fn is_known<T>(probe: &Probe<T>) -> bool {
    matches!(probe, Probe::Known(_))
}

/// Collects the non-blocking warnings shared by advice and removal decisions.
fn collect_warnings(observation: &Observation, policy: &Policy, warnings: &mut Vec<Warning>) {
    if let Some(size) = probe_value(&observation.size)
        && size.bytes >= policy.size_warning_bytes
    {
        warnings.push(Warning::SizeAtLeast { bytes: size.bytes });
    }
    match probe_value(&observation.integration).copied() {
        Some(Integration::Unmerged) => warnings.push(Warning::Unmerged),
        Some(Integration::AncestorMerged) => {}
        Some(Integration::Unknown) | None => warnings.push(Warning::IntegrationUnknown),
    }
    let reasons = [
        incomplete_reason(&observation.live_processes),
        incomplete_reason(&observation.status),
        incomplete_reason(&observation.submodules),
        incomplete_reason(&observation.integration),
        incomplete_reason(&observation.size),
    ];
    for reason in reasons.into_iter().flatten() {
        warnings.push(Warning::ProbeIncomplete {
            reason: reason.to_owned(),
        });
    }
}

/// True when `ignored` falls under an approved disposable path.
///
/// Coverage is a prefix match on path components: `target` covers
/// `target/debug` and `target/` but not `targetx`. Entries that are empty,
/// dot-only or absolute approve nothing, because `disposable_paths` are
/// worktree-relative and an over-broad entry must never silently approve
/// ignored work.
fn is_disposable(ignored: &str, disposable: &[PathBuf]) -> bool {
    let ignored = Path::new(ignored);
    disposable.iter().any(|approved| {
        matches!(
            approved.components().next(),
            Some(std::path::Component::Normal(_))
        ) && ignored.strip_prefix(approved).is_ok()
    })
}

/// Lowercase hex encoding of a byte slice.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Test assertions")]
mod tests {
    use super::*;

    #[test]
    fn repo_id_is_stable_and_prefixable() {
        let a = RepoId::from_common_dir(Path::new("/repo/.git"));
        assert_eq!(a, RepoId::from_common_dir(Path::new("/repo/.git")));
        assert_ne!(a, RepoId::from_common_dir(Path::new("/repo2/.git")));
        assert_eq!(a.id12().len(), 12);
        assert_eq!(a.as_str().len(), 64);
        assert_eq!(RepoId::parse(a.as_str()).unwrap(), a);
        assert_eq!(RepoId::parse("ABC"), Err(ValidationError::IdentityFormat));
    }

    #[test]
    fn name_and_label_rules() {
        assert!(WorktreeName::parse("task-42").is_ok());
        assert_eq!(
            WorktreeName::parse("task-42").unwrap().default_branch(),
            "aw/task-42"
        );
        assert_eq!(
            WorktreeName::parse("x".repeat(65).as_str()),
            Err(ValidationError::NameLength)
        );
        assert_eq!(
            WorktreeName::parse("-lead"),
            Err(ValidationError::NameCharset)
        );
        assert_eq!(
            WorktreeName::parse("Upper"),
            Err(ValidationError::NameCharset)
        );
        assert!(validate_label("my.repo-1").is_ok());
        assert_eq!(validate_label("-bad"), Err(ValidationError::LabelCharset));
        assert_eq!(
            validate_label(&"l".repeat(33)),
            Err(ValidationError::LabelLength)
        );
    }

    #[test]
    fn branch_rules() {
        assert!(validate_branch("aw/task-1").is_ok());
        assert!(validate_branch("feature/x").is_ok());
        assert_eq!(validate_branch(""), Err(ValidationError::BranchUnsafe));
        assert_eq!(validate_branch("-lead"), Err(ValidationError::BranchUnsafe));
        assert_eq!(validate_branch("a..b"), Err(ValidationError::BranchUnsafe));
        assert_eq!(
            validate_branch("refs/heads/x.lock"),
            Err(ValidationError::BranchUnsafe)
        );
        assert_eq!(
            validate_branch("has space"),
            Err(ValidationError::BranchUnsafe)
        );
    }

    fn fp(disposable: &[PathBuf], record_revision: u64, policy_revision: u64) -> Fingerprint {
        fingerprint(&FingerprintInput {
            path: Path::new("/w"),
            head: "deadbeef",
            branch: "aw/task-1",
            status_digest: "cafe01",
            disposable_paths: disposable,
            record_revision,
            policy_revision,
        })
    }

    #[test]
    fn fingerprint_is_deterministic_and_sensitive() {
        let paths = vec![PathBuf::from("target/")];
        assert_eq!(fp(&paths, 3, 1), fp(&paths.clone(), 3, 1));
        assert_ne!(fp(&paths, 3, 1), fp(&paths, 4, 1));
        assert_ne!(fp(&paths, 3, 1), fp(&paths, 3, 2));
        let duplicated = vec![PathBuf::from("target/"), PathBuf::from("target/")];
        assert_eq!(fp(&paths, 3, 1), fp(&duplicated, 3, 1));
        assert_eq!(fp(&paths, 3, 1).as_str().len(), 64);
    }

    fn sample_observation() -> Observation {
        Observation {
            id: WorktreeId {
                repo: RepoId::from_common_dir(Path::new("/repo/.git")),
                name: "task-1".to_owned(),
            },
            observed_at: 1_000,
            registration: Registration {
                path: PathBuf::from("/w/task-1"),
                head: Some("deadbeef".to_owned()),
                branch: Some("refs/heads/aw/task-1".to_owned()),
                detached: false,
                bare: false,
                locked: None,
                prunable: None,
                is_main: false,
            },
            activity_signals: ActivitySignals::default(),
            live_processes: Probe::NotChecked,
            status: Probe::NotChecked,
            submodules: Probe::NotChecked,
            integration: Probe::NotChecked,
            size: Probe::NotChecked,
        }
    }

    /// Fully known, clean observation: the baseline every table row tweaks.
    fn clean_observation() -> Observation {
        Observation {
            live_processes: Probe::Known(false),
            status: Probe::Known(StatusFacts {
                staged: 0,
                unstaged: 0,
                untracked: 0,
                conflicts: 0,
                ignored: Vec::new(),
                digest: "cafe01".to_owned(),
            }),
            submodules: Probe::Known(SubmoduleFacts {
                dirty: false,
                unsupported: false,
            }),
            integration: Probe::Known(Integration::AncestorMerged),
            size: Probe::NotChecked,
            ..sample_observation()
        }
    }

    /// Clones a base observation and applies one tweak.
    fn with(base: &Observation, tweak: impl FnOnce(&mut Observation)) -> Observation {
        let mut observation = base.clone();
        tweak(&mut observation);
        observation
    }

    /// Tweaks the facts of a known status probe; panics outside tests.
    fn with_status(base: &Observation, tweak: impl FnOnce(&mut StatusFacts)) -> Observation {
        with(base, |observation| {
            if let Probe::Known(facts) = &mut observation.status {
                tweak(facts);
            }
        })
    }

    /// A stored record for the sample worktree at revision 3.
    fn sample_record() -> Record {
        Record {
            schema_version: RECORD_SCHEMA_VERSION,
            repo_id: RepoId::from_common_dir(Path::new("/repo/.git"))
                .as_str()
                .to_owned(),
            name: "task-1".to_owned(),
            path: PathBuf::from("/w/task-1"),
            branch: Some("refs/heads/aw/task-1".to_owned()),
            base_ref: None,
            base_oid: None,
            created_at: 500,
            creator: "harness".to_owned(),
            revision: 3,
            removal_started: None,
        }
    }

    /// The fingerprint `assess_removal` should compute for this setup.
    fn expected_fingerprint(
        observation: &Observation,
        record: Option<&Record>,
        request: &RemovalRequest,
        digest: &str,
    ) -> Fingerprint {
        fingerprint(&FingerprintInput {
            path: &observation.registration.path,
            head: observation.registration.head.as_deref().unwrap_or(""),
            branch: observation.registration.branch.as_deref().unwrap_or(""),
            status_digest: digest,
            disposable_paths: &request.disposable_paths,
            record_revision: record.map_or(0, |r| r.revision),
            policy_revision: Policy::default().revision,
        })
    }

    #[test]
    fn classify_activity_bands() {
        let now = 10_000_000_u64;
        let hour = 3_600_u64;
        let day = 24 * hour;
        let signals =
            |head: Option<u64>, index: Option<u64>, reflog: Option<u64>| ActivitySignals {
                head_mtime: head,
                index_mtime: index,
                last_reflog_entry: reflog,
            };
        let cases = [
            (
                "live process wins",
                Probe::Known(true),
                signals(None, None, None),
                Activity::Active,
            ),
            (
                "fresh signal",
                Probe::Known(false),
                signals(Some(now - hour), None, None),
                Activity::Recent,
            ),
            (
                "recent boundary",
                Probe::Known(false),
                signals(None, Some(now - 86_399), None),
                Activity::Recent,
            ),
            (
                "between recent and idle stays recent",
                Probe::Known(false),
                signals(Some(now - 25 * hour), None, None),
                Activity::Recent,
            ),
            (
                "newest signal decides",
                Probe::Known(false),
                signals(Some(now - 31 * day), Some(now - hour), Some(now - 8 * day)),
                Activity::Recent,
            ),
            (
                "idle boundary",
                Probe::Known(false),
                signals(None, None, Some(now - 7 * day)),
                Activity::IdleCandidate,
            ),
            (
                "stale boundary",
                Probe::Known(false),
                signals(Some(now - 30 * day), None, None),
                Activity::StaleCandidate,
            ),
            (
                "no signals is unknown",
                Probe::Known(false),
                signals(None, None, None),
                Activity::Unknown,
            ),
            (
                "unchecked process probe does not fake knowledge",
                Probe::NotChecked,
                signals(None, None, None),
                Activity::Unknown,
            ),
        ];
        for (name, live, signals, expected) in cases {
            let observation = with(&clean_observation(), |o| {
                o.live_processes = live;
                o.activity_signals = signals;
            });
            assert_eq!(
                classify(&observation, &Policy::default(), now).activity,
                expected,
                "case {name}"
            );
        }
    }

    #[test]
    fn classify_warnings() {
        let policy = Policy {
            size_warning_bytes: 100,
            ..Policy::default()
        };
        let size_of = |bytes: u64, quality: SizeQuality| {
            with(&clean_observation(), |o| {
                o.size = Probe::Known(Size { bytes, quality })
            })
        };
        let warns_at_least = |observation: &Observation| {
            classify(observation, &policy, 0)
                .warnings
                .iter()
                .any(|warning| matches!(warning, Warning::SizeAtLeast { bytes } if *bytes >= 100))
        };
        assert!(warns_at_least(&size_of(100, SizeQuality::Complete)));
        // A lower bound at or above the threshold also warns.
        assert!(warns_at_least(&size_of(150, SizeQuality::LowerBound)));
        assert!(!warns_at_least(&size_of(99, SizeQuality::Complete)));

        let unmerged = with(&clean_observation(), |o| {
            o.integration = Probe::Known(Integration::Unmerged)
        });
        assert!(
            classify(&unmerged, &policy, 0)
                .warnings
                .contains(&Warning::Unmerged)
        );
        let unchecked = with(&clean_observation(), |o| o.integration = Probe::NotChecked);
        assert!(
            classify(&unchecked, &policy, 0)
                .warnings
                .contains(&Warning::IntegrationUnknown)
        );
        let unknown_integration = with(&clean_observation(), |o| {
            o.integration = Probe::Known(Integration::Unknown)
        });
        assert!(
            classify(&unknown_integration, &policy, 0)
                .warnings
                .contains(&Warning::IntegrationUnknown)
        );

        let incomplete = with(&clean_observation(), |o| {
            o.status = Probe::Incomplete {
                evidence: StatusFacts {
                    staged: 0,
                    unstaged: 0,
                    untracked: 0,
                    conflicts: 0,
                    ignored: Vec::new(),
                    digest: "cafe01".to_owned(),
                },
                reason: "output cap hit".to_owned(),
            }
        });
        assert!(
            classify(&incomplete, &policy, 0)
                .warnings
                .contains(&Warning::ProbeIncomplete {
                    reason: "output cap hit".to_owned()
                })
        );
    }

    #[test]
    fn assess_removal_table() {
        let policy = Policy::default();
        let clean = clean_observation();
        let record = sample_record();
        let unmerged_observation = with(&clean, |o| {
            o.integration = Probe::Known(Integration::Unmerged)
        });
        let covered_request = RemovalRequest {
            disposable_paths: vec![PathBuf::from("target")],
            ..RemovalRequest::default()
        };
        // One veto table row: case name, observation, stored record, request,
        // and the exact veto list expected.
        type VetoRow = (
            &'static str,
            Observation,
            Option<Record>,
            RemovalRequest,
            Vec<Veto>,
        );
        let cases: Vec<VetoRow> = vec![
            (
                "main worktree",
                with(&clean, |o| o.registration.is_main = true),
                None,
                RemovalRequest::default(),
                vec![Veto::MainWorktree],
            ),
            (
                "bare repository",
                with(&clean, |o| o.registration.bare = true),
                None,
                RemovalRequest::default(),
                vec![Veto::BareRepository],
            ),
            (
                "git locked",
                with(&clean, |o| {
                    o.registration.locked = Some("pinned".to_owned())
                }),
                None,
                RemovalRequest::default(),
                vec![Veto::GitLocked],
            ),
            (
                "staged changes",
                with_status(&clean, |f| f.staged = 1),
                None,
                RemovalRequest::default(),
                vec![Veto::Dirty],
            ),
            (
                "unstaged changes",
                with_status(&clean, |f| f.unstaged = 1),
                None,
                RemovalRequest::default(),
                vec![Veto::Dirty],
            ),
            (
                "untracked files",
                with_status(&clean, |f| f.untracked = 2),
                None,
                RemovalRequest::default(),
                vec![Veto::UntrackedFiles],
            ),
            (
                "conflicts",
                with_status(&clean, |f| f.conflicts = 1),
                None,
                RemovalRequest::default(),
                vec![Veto::Conflicts],
            ),
            (
                "dirty submodules",
                with(&clean, |o| {
                    o.submodules = Probe::Known(SubmoduleFacts {
                        dirty: true,
                        unsupported: false,
                    })
                }),
                None,
                RemovalRequest::default(),
                vec![Veto::DirtySubmodules],
            ),
            (
                "unsupported submodules",
                with(&clean, |o| {
                    o.submodules = Probe::Known(SubmoduleFacts {
                        dirty: false,
                        unsupported: true,
                    })
                }),
                None,
                RemovalRequest::default(),
                vec![Veto::DirtySubmodules],
            ),
            (
                "ignored outside disposable paths",
                with_status(&clean, |f| f.ignored = vec!["build/".to_owned()]),
                None,
                covered_request.clone(),
                vec![Veto::IgnoredNotDisposable {
                    paths: vec!["build/".to_owned()],
                }],
            ),
            (
                "ignored covered by component prefix",
                with_status(&clean, |f| f.ignored = vec!["target/debug/log".to_owned()]),
                None,
                covered_request.clone(),
                Vec::new(),
            ),
            (
                "string prefix is not component coverage",
                with_status(&clean, |f| f.ignored = vec!["targetx".to_owned()]),
                None,
                covered_request.clone(),
                vec![Veto::IgnoredNotDisposable {
                    paths: vec!["targetx".to_owned()],
                }],
            ),
            (
                "live process",
                with(&clean, |o| o.live_processes = Probe::Known(true)),
                None,
                RemovalRequest::default(),
                vec![Veto::LiveProcess],
            ),
            (
                "incomplete process evidence still vetoes",
                with(&clean, |o| {
                    o.live_processes = Probe::Incomplete {
                        evidence: true,
                        reason: "lsof denied".to_owned(),
                    }
                }),
                None,
                RemovalRequest::default(),
                vec![Veto::LiveProcess, Veto::ProbeUnknown],
            ),
            (
                "status not checked",
                with(&clean, |o| o.status = Probe::NotChecked),
                None,
                RemovalRequest::default(),
                vec![Veto::ProbeUnknown],
            ),
            (
                "status unavailable",
                with(&clean, |o| {
                    o.status = Probe::Unavailable {
                        code: "timeout".to_owned(),
                    }
                }),
                None,
                RemovalRequest::default(),
                vec![Veto::ProbeUnknown],
            ),
            (
                "status incomplete",
                with(&clean, |o| {
                    o.status = Probe::Incomplete {
                        evidence: StatusFacts {
                            staged: 0,
                            unstaged: 0,
                            untracked: 0,
                            conflicts: 0,
                            ignored: Vec::new(),
                            digest: "cafe01".to_owned(),
                        },
                        reason: "output cap hit".to_owned(),
                    }
                }),
                None,
                RemovalRequest::default(),
                vec![Veto::ProbeUnknown],
            ),
            (
                "submodules not checked",
                with(&clean, |o| o.submodules = Probe::NotChecked),
                None,
                RemovalRequest::default(),
                vec![Veto::ProbeUnknown],
            ),
            (
                "integration not checked",
                with(&clean, |o| o.integration = Probe::NotChecked),
                None,
                RemovalRequest::default(),
                vec![Veto::ProbeUnknown],
            ),
            (
                "integration unavailable",
                with(&clean, |o| {
                    o.integration = Probe::Unavailable {
                        code: "no-ref".to_owned(),
                    }
                }),
                None,
                RemovalRequest::default(),
                vec![Veto::ProbeUnknown],
            ),
            (
                "unmerged without confirmation",
                unmerged_observation.clone(),
                None,
                RemovalRequest::default(),
                vec![Veto::Unmerged],
            ),
            (
                "unmerged with explicit confirmation",
                unmerged_observation.clone(),
                None,
                RemovalRequest {
                    allow_unmerged: true,
                    ..RemovalRequest::default()
                },
                Vec::new(),
            ),
            (
                "fingerprint mismatch",
                clean.clone(),
                None,
                RemovalRequest {
                    expected_fingerprint: Some(expected_fingerprint(
                        &clean,
                        None,
                        &RemovalRequest::default(),
                        "tampered",
                    )),
                    ..RemovalRequest::default()
                },
                vec![Veto::FingerprintMismatch],
            ),
            (
                "fingerprint match",
                clean.clone(),
                None,
                RemovalRequest {
                    expected_fingerprint: Some(expected_fingerprint(
                        &clean,
                        None,
                        &RemovalRequest::default(),
                        "cafe01",
                    )),
                    ..RemovalRequest::default()
                },
                Vec::new(),
            ),
            (
                "fingerprint binds the record revision",
                clean.clone(),
                Some(record.clone()),
                RemovalRequest {
                    expected_fingerprint: Some(expected_fingerprint(
                        &clean,
                        None,
                        &RemovalRequest::default(),
                        "cafe01",
                    )),
                    ..RemovalRequest::default()
                },
                vec![Veto::FingerprintMismatch],
            ),
            (
                "clean merged foreign worktree with no record",
                clean.clone(),
                None,
                RemovalRequest::default(),
                Vec::new(),
            ),
        ];
        for (name, observation, record, request, expected) in cases {
            let decision = assess_removal(&observation, record.as_ref(), &request, &policy);
            assert_eq!(decision.vetoes, expected, "case {name}");
            assert!(
                !decision
                    .vetoes
                    .iter()
                    .any(|veto| matches!(veto, Veto::NotImplemented)),
                "case {name} still returns not_implemented"
            );
        }
    }

    #[test]
    fn assess_removal_fingerprint_presence() {
        let policy = Policy::default();
        let clean = clean_observation();
        let decision = assess_removal(&clean, None, &RemovalRequest::default(), &policy);
        assert_eq!(
            decision.fingerprint,
            Some(expected_fingerprint(
                &clean,
                None,
                &RemovalRequest::default(),
                "cafe01"
            ))
        );
        // Incomplete status still yields a fingerprint from its evidence.
        let incomplete = with(&clean, |o| {
            o.status = Probe::Incomplete {
                evidence: StatusFacts {
                    staged: 0,
                    unstaged: 0,
                    untracked: 0,
                    conflicts: 0,
                    ignored: Vec::new(),
                    digest: "cafe01".to_owned(),
                },
                reason: "output cap hit".to_owned(),
            }
        });
        assert_eq!(
            assess_removal(&incomplete, None, &RemovalRequest::default(), &policy).fingerprint,
            decision.fingerprint
        );
        // No status digest means no fingerprint: never a fabricated one.
        let blind = with(&clean, |o| o.status = Probe::NotChecked);
        assert!(
            assess_removal(&blind, None, &RemovalRequest::default(), &policy)
                .fingerprint
                .is_none()
        );
    }

    #[test]
    fn assess_removal_carries_record_warnings() {
        let policy = Policy::default();
        let clean = clean_observation();
        let mut record = sample_record();
        record.removal_started = Some(RemovalStarted {
            fingerprint: Fingerprint::parse(&"0".repeat(64)).unwrap(),
            at: 999,
        });
        let decision = assess_removal(&clean, Some(&record), &RemovalRequest::default(), &policy);
        assert!(decision.vetoes.is_empty());
        assert!(decision.warnings.contains(&Warning::RemovalStarted));

        let unmerged = with(&clean, |o| {
            o.integration = Probe::Known(Integration::Unmerged)
        });
        let decision = assess_removal(&unmerged, None, &RemovalRequest::default(), &policy);
        assert!(decision.warnings.contains(&Warning::Unmerged));
    }

    #[test]
    fn unknown_is_never_clean() {
        // Every unknown axis that matters produces a veto, never silence.
        let policy = Policy::default();
        let observation = sample_observation();
        let decision = assess_removal(&observation, None, &RemovalRequest::default(), &policy);
        assert_eq!(decision.vetoes, vec![Veto::ProbeUnknown]);
    }
}
