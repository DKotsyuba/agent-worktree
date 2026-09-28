//! Local state contracts: layout resolution, records, registry, locking and discovery.
//!
//! State lives under the product home as small versioned JSON files with atomic
//! replacement and a per-repository advisory lock. This module never interprets
//! Git facts; it stores and reads typed records. Bodies are frozen substitutes
//! that return `StoreErrorCode::NotImplemented`; nothing is written yet.

use crate::worktree::{Budget, Fingerprint, Record};
use std::path::{Path, PathBuf};

/// Stable error class for a failed state operation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreErrorCode {
    /// The contract is not implemented yet.
    NotImplemented,
    /// Filesystem I/O failed.
    Io,
    /// The repository lock could not be acquired within the deadline.
    LockTimeout,
    /// The record revision moved between read and write.
    RevisionConflict,
    /// The stored schema version is not one this build understands.
    SchemaIncompatible,
    /// A stored record is not valid JSON for its schema.
    CorruptRecord,
    /// A record exceeds the 8 KiB cap.
    RecordTooLarge,
    /// The configuration file is invalid.
    InvalidConfig,
}

impl StoreErrorCode {
    /// Returns the stable snake_case code used in tool replies.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotImplemented => "not_implemented",
            Self::Io => "io",
            Self::LockTimeout => "lock_timeout",
            Self::RevisionConflict => "revision_conflict",
            Self::SchemaIncompatible => "schema_incompatible",
            Self::CorruptRecord => "corrupt_record",
            Self::RecordTooLarge => "record_too_large",
            Self::InvalidConfig => "invalid_config",
        }
    }
}

/// Typed state error carrying a stable code and a bounded detail.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StoreError {
    /// Stable error class.
    pub code: StoreErrorCode,
    /// Bounded human-readable detail.
    pub detail: String,
}

impl StoreError {
    /// Builds an error from a code and detail text.
    #[must_use]
    pub fn new(code: StoreErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    /// Builds the substitute error used by the frozen contract bodies.
    #[must_use]
    pub fn not_implemented() -> Self {
        Self::new(
            StoreErrorCode::NotImplemented,
            "store contract not implemented",
        )
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.detail)
    }
}

impl std::error::Error for StoreError {}

/// Parsed `config.toml` contents; absent file means defaults.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Config {
    /// Worktree root override (`[storage] root`).
    pub root: Option<PathBuf>,
    /// Roots scanned for repository discovery (`[discovery] roots`).
    pub discovery_roots: Vec<PathBuf>,
}

/// Resolved product home, worktree root and configuration.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Layout {
    /// Product home, `~/.agent-worktree/` by default.
    pub home: PathBuf,
    /// Worktree root, `<home>/worktrees` by default.
    pub root: PathBuf,
    /// Parsed configuration; defaults when no `config.toml` exists.
    pub config: Config,
}

/// Resolves home and worktree root.
///
/// Precedence: `AGENT_WORKTREE_HOME` beats the platform home; `AGENT_WORKTREE_ROOT`
/// beats `config.toml`, which beats `<home>/worktrees`.
pub fn resolve_layout(
    env_home: Option<PathBuf>,
    env_root: Option<PathBuf>,
    platform_home: PathBuf,
) -> Result<Layout, StoreError> {
    let _ = (env_home, env_root, platform_home);
    Err(StoreError::not_implemented())
}

/// Guard holding the per-repository advisory lock; released on drop.
#[derive(Clone, Copy, Debug)]
pub struct RepoLockGuard {
    /// Placeholder so the type cannot be constructed outside this module.
    _private: (),
}

/// Acquires the exclusive per-repository lock under `<home>/state/v1/repos/<repo-id>/`.
///
/// The lock serializes cooperating product processes only; a foreign process can
/// still start writing after inspection. Waiting past `budget.deadline` fails
/// with `StoreErrorCode::LockTimeout`.
pub async fn lock_repo(
    home: &Path,
    repo_id: &str,
    budget: &Budget,
) -> Result<RepoLockGuard, StoreError> {
    let _ = (home, repo_id, budget);
    Err(StoreError::not_implemented())
}

/// Reads one worktree record; `Ok(None)` when no record is stored.
pub fn read_record(home: &Path, repo_id: &str, name: &str) -> Result<Option<Record>, StoreError> {
    let _ = (home, repo_id, name);
    Err(StoreError::not_implemented())
}

/// Atomically replaces one record after checking its revision.
///
/// The caller supplies the record with `revision` set to the observed value;
/// the write stores `revision + 1` and returns the stored record. A moved
/// revision, an incompatible `schema_version`, an absent parent record when one
/// was implied, or a record over 8 KiB refuses the write.
pub fn replace_record(
    home: &Path,
    record: &Record,
    expected_revision: u64,
) -> Result<Record, StoreError> {
    let _ = (home, record, expected_revision);
    Err(StoreError::not_implemented())
}

/// Writes `removal_started` into a record before the removal is dispatched.
///
/// This is a revision-checked replace; an interrupted removal therefore stays
/// visible after a restart.
pub fn mark_removal_started(
    home: &Path,
    repo_id: &str,
    name: &str,
    fingerprint: &Fingerprint,
    expected_revision: u64,
    at: u64,
) -> Result<Record, StoreError> {
    let _ = (home, repo_id, name, fingerprint, expected_revision, at);
    Err(StoreError::not_implemented())
}

/// One known-repo registry entry.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KnownRepo {
    /// Repository identity (64 hex characters).
    pub repo_id: String,
    /// Canonical absolute Git common directory.
    pub common_dir: PathBuf,
    /// Bounded ASCII label used in directory names.
    pub label: String,
    /// Integration ref used for ancestry checks (for example `main`).
    pub integration_ref: String,
    /// Unix seconds when the repository was first recorded.
    pub registered_at: u64,
}

/// Reads the known-repo registry; empty when nothing is recorded.
pub fn read_registry(home: &Path) -> Result<Vec<KnownRepo>, StoreError> {
    let _ = home;
    Err(StoreError::not_implemented())
}

/// Adds or refreshes one registry entry; idempotent for identical identities.
pub fn add_known_repo(home: &Path, entry: &KnownRepo) -> Result<(), StoreError> {
    let _ = (home, entry);
    Err(StoreError::not_implemented())
}

/// How a discovered repository was found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DiscoverySource {
    /// Found by scanning a configured root.
    Root,
    /// Already present in the known-repo registry.
    Registry,
}

/// One discovered repository.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiscoveredRepo {
    /// Canonical absolute Git common directory.
    pub common_dir: PathBuf,
    /// How the repository was found.
    pub source: DiscoverySource,
}

/// Bounded result of a discovery pass.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct DiscoveryReport {
    /// Repositories found, deduplicated by common directory.
    pub repos: Vec<DiscoveredRepo>,
    /// Directory entries inspected while scanning.
    pub scanned_entries: usize,
    /// Paths skipped as symlinks, unreadable or excluded (`target/`, `node_modules/`).
    pub skipped: Vec<PathBuf>,
    /// True when the entry, depth or deadline budget ran out before covering the scope.
    pub budget_exhausted: bool,
}

/// Discovers repositories from the configured roots.
///
/// Scans to depth at most 2, never follows symlinks, skips `target/` and
/// `node_modules/`, and stops at `budget`. Repositories outside the scanned
/// scope remain unknown, not absent.
pub fn discover(layout: &Layout, budget: &Budget) -> Result<DiscoveryReport, StoreError> {
    let _ = (layout, budget);
    Err(StoreError::not_implemented())
}
