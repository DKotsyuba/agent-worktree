//! Local state: layout resolution, records, registry, locking and discovery.
//!
//! State lives under the product home as small versioned JSON files with atomic
//! replacement and a per-repository advisory lock. This module never interprets
//! Git facts; it stores and reads typed records.

use crate::worktree::{Budget, Fingerprint, RECORD_SCHEMA_VERSION, Record, RemovalStarted};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// On-disk size cap for one record, matching the architecture's 8 KiB limit.
const MAX_RECORD_BYTES: usize = 8 * 1024;
/// Pause between attempts while polling an occupied repository lock.
const LOCK_POLL: Duration = Duration::from_millis(10);

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
/// beats `config.toml`, which beats `<home>/worktrees`. The configuration is read
/// from `<home>/config.toml`; an absent file means defaults, and an invalid or
/// unknown-key file refuses resolution with `StoreErrorCode::InvalidConfig`.
pub fn resolve_layout(
    env_home: Option<PathBuf>,
    env_root: Option<PathBuf>,
    platform_home: PathBuf,
) -> Result<Layout, StoreError> {
    let home = env_home.unwrap_or(platform_home);
    let config = read_config(&home)?;
    let root = env_root
        .or(config.root.clone())
        .unwrap_or_else(|| home.join("worktrees"));
    Ok(Layout { home, root, config })
}

/// Reads `<home>/config.toml`; an absent file yields default configuration.
fn read_config(home: &Path) -> Result<Config, StoreError> {
    let path = home.join("config.toml");
    match fs::read_to_string(&path) {
        Ok(text) => parse_config(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(StoreError::new(
            StoreErrorCode::Io,
            format!("read {}: {e}", path.display()),
        )),
    }
}

/// Parses `config.toml` text, rejecting any key this build does not know.
fn parse_config(text: &str) -> Result<Config, StoreError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawStorage {
        root: Option<PathBuf>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawDiscovery {
        roots: Option<Vec<PathBuf>>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct RawConfig {
        storage: Option<RawStorage>,
        discovery: Option<RawDiscovery>,
    }
    let raw: RawConfig = toml::from_str(text).map_err(|e| {
        let detail: String = e.to_string().chars().take(200).collect();
        StoreError::new(StoreErrorCode::InvalidConfig, detail)
    })?;
    Ok(Config {
        root: raw.storage.and_then(|storage| storage.root),
        discovery_roots: raw
            .discovery
            .and_then(|discovery| discovery.roots)
            .unwrap_or_default(),
    })
}

/// Guard holding the per-repository advisory lock.
///
/// The lock is released when the last clone drops; cloning shares one open lock
/// file instead of acquiring a second lock. A foreign process that does not
/// cooperate is not stopped by this lock.
#[derive(Clone, Debug)]
pub struct RepoLockGuard {
    /// Placeholder so the type cannot be constructed outside this module.
    _private: (),
    /// Open lock file keeping the advisory lock alive until the last drop;
    /// held for its closing side effect, never read by value.
    _lock_file: Option<Arc<File>>,
}

/// Acquires the exclusive per-repository lock on `<home>/state/v1/repos/<repo-id>/lock`.
///
/// The lock serializes cooperating product processes only; a foreign process can
/// still start writing after inspection. Attempts are polled every 10 ms until
/// the lock is free; waiting past `budget.deadline` fails with
/// `StoreErrorCode::LockTimeout`. Creating the lock file or its directories can
/// fail with `StoreErrorCode::Io`.
pub async fn lock_repo(
    home: &Path,
    repo_id: &str,
    budget: &Budget,
) -> Result<RepoLockGuard, StoreError> {
    let lock_path = repo_dir(home, repo_id).join("lock");
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            StoreError::new(
                StoreErrorCode::Io,
                format!("create {}: {e}", parent.display()),
            )
        })?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| {
            StoreError::new(
                StoreErrorCode::Io,
                format!("open {}: {e}", lock_path.display()),
            )
        })?;
    loop {
        if file.try_lock_exclusive().is_ok() {
            return Ok(RepoLockGuard {
                _private: (),
                _lock_file: Some(Arc::new(file)),
            });
        }
        if Instant::now() >= budget.deadline {
            return Err(StoreError::new(
                StoreErrorCode::LockTimeout,
                format!("lock busy: {}", lock_path.display()),
            ));
        }
        tokio::time::sleep(LOCK_POLL).await;
    }
}

/// Per-repository state directory `<home>/state/v1/repos/<repo-id>/`.
fn repo_dir(home: &Path, repo_id: &str) -> PathBuf {
    home.join("state").join("v1").join("repos").join(repo_id)
}

/// Record file path `<home>/state/v1/repos/<repo-id>/<name>.json`.
fn record_path(home: &Path, repo_id: &str, name: &str) -> PathBuf {
    repo_dir(home, repo_id).join(format!("{name}.json"))
}

/// Reads one worktree record; `Ok(None)` when no record is stored.
///
/// A missing file is not an error. An unreadable file fails with `Io`; a file
/// over 8 KiB fails with `RecordTooLarge`; invalid JSON or a record naming a
/// different repository/worktree fails with `CorruptRecord`; a schema version
/// this build does not understand fails with `SchemaIncompatible`.
pub fn read_record(home: &Path, repo_id: &str, name: &str) -> Result<Option<Record>, StoreError> {
    let path = record_path(home, repo_id, name);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(StoreError::new(
                StoreErrorCode::Io,
                format!("read {}: {e}", path.display()),
            ));
        }
    };
    let record = parse_record(&bytes)?;
    if record.repo_id != repo_id || record.name != name {
        return Err(StoreError::new(
            StoreErrorCode::CorruptRecord,
            format!("record at {} names a different worktree", path.display()),
        ));
    }
    Ok(Some(record))
}

/// Decodes record bytes with size, syntax, schema and identity checks.
fn parse_record(bytes: &[u8]) -> Result<Record, StoreError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(StoreError::new(
            StoreErrorCode::RecordTooLarge,
            format!("{} bytes exceeds the 8 KiB cap", bytes.len()),
        ));
    }
    let record: Record = serde_json::from_slice(bytes).map_err(|e| {
        StoreError::new(
            StoreErrorCode::CorruptRecord,
            format!("invalid record JSON: {e}"),
        )
    })?;
    if record.schema_version != RECORD_SCHEMA_VERSION {
        return Err(StoreError::new(
            StoreErrorCode::SchemaIncompatible,
            format!(
                "schema version {} is not {}",
                record.schema_version, RECORD_SCHEMA_VERSION
            ),
        ));
    }
    Ok(record)
}

/// Atomically replaces one record after checking its revision.
///
/// The caller supplies the record with `revision` set to the observed value;
/// the write stores `revision + 1` and returns the stored record. A moved
/// revision (or claiming a revision for a record that does not exist) refuses
/// with `RevisionConflict`; an incompatible `schema_version` refuses with
/// `SchemaIncompatible`; a serialized record over 8 KiB refuses with
/// `RecordTooLarge` and nothing is written. The write is a temp-file, fsync,
/// rename and directory-fsync replacement in the record's own directory.
pub fn replace_record(
    home: &Path,
    record: &Record,
    expected_revision: u64,
) -> Result<Record, StoreError> {
    if record.schema_version != RECORD_SCHEMA_VERSION {
        return Err(StoreError::new(
            StoreErrorCode::SchemaIncompatible,
            format!(
                "schema version {} is not {}",
                record.schema_version, RECORD_SCHEMA_VERSION
            ),
        ));
    }
    match read_record(home, &record.repo_id, &record.name)? {
        Some(existing) if existing.revision != expected_revision => {
            return Err(StoreError::new(
                StoreErrorCode::RevisionConflict,
                format!(
                    "stored revision {} moved past expected {expected_revision}",
                    existing.revision
                ),
            ));
        }
        Some(_) => {}
        None if expected_revision != 0 => {
            return Err(StoreError::new(
                StoreErrorCode::RevisionConflict,
                format!("no stored record for observed revision {expected_revision}"),
            ));
        }
        None => {}
    }
    let mut stored = record.clone();
    stored.revision = expected_revision + 1;
    let bytes = serde_json::to_vec(&stored).map_err(|e| {
        StoreError::new(
            StoreErrorCode::Io,
            format!("serialize record {}: {e}", record.name),
        )
    })?;
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(StoreError::new(
            StoreErrorCode::RecordTooLarge,
            format!("{} bytes exceeds the 8 KiB cap", bytes.len()),
        ));
    }
    atomic_write(&record_path(home, &record.repo_id, &record.name), &bytes)?;
    Ok(stored)
}

/// Writes `removal_started` into a record before the removal is dispatched.
///
/// This is a revision-checked replace, so an interrupted removal stays visible
/// after a restart. A missing record refuses with `RevisionConflict` rather
/// than creating one; other failures match `replace_record`.
pub fn mark_removal_started(
    home: &Path,
    repo_id: &str,
    name: &str,
    fingerprint: &Fingerprint,
    expected_revision: u64,
    at: u64,
) -> Result<Record, StoreError> {
    let mut record = read_record(home, repo_id, name)?.ok_or_else(|| {
        StoreError::new(
            StoreErrorCode::RevisionConflict,
            format!("no stored record for {repo_id}/{name}"),
        )
    })?;
    record.removal_started = Some(RemovalStarted {
        fingerprint: fingerprint.clone(),
        at,
    });
    replace_record(home, &record, expected_revision)
}

/// Writes `bytes` to `path` atomically: temp file in the same directory, fsync,
/// rename, fsync of the directory. A failed attempt removes the temp file.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let dir = path.parent().ok_or_else(|| {
        StoreError::new(
            StoreErrorCode::Io,
            format!("{} has no parent directory", path.display()),
        )
    })?;
    fs::create_dir_all(dir).map_err(|e| {
        StoreError::new(StoreErrorCode::Io, format!("create {}: {e}", dir.display()))
    })?;
    let file_name = path.file_name().map_or_else(
        || "state".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let temp = dir.join(format!(".{file_name}.{}.tmp", temp_suffix()));
    let write = || -> Result<(), StoreError> {
        let mut file = File::create(&temp)
            .map_err(|e| StoreError::new(StoreErrorCode::Io, format!("create temp: {e}")))?;
        file.write_all(bytes)
            .map_err(|e| StoreError::new(StoreErrorCode::Io, format!("write temp: {e}")))?;
        file.sync_all()
            .map_err(|e| StoreError::new(StoreErrorCode::Io, format!("sync temp: {e}")))?;
        fs::rename(&temp, path).map_err(|e| {
            StoreError::new(
                StoreErrorCode::Io,
                format!("rename into {}: {e}", path.display()),
            )
        })
    };
    if let Err(e) = write() {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    let dir_handle = File::open(dir)
        .map_err(|e| StoreError::new(StoreErrorCode::Io, format!("open {}: {e}", dir.display())))?;
    dir_handle
        .sync_all()
        .map_err(|e| StoreError::new(StoreErrorCode::Io, format!("sync dir: {e}")))?;
    Ok(())
}

/// Unique temp-file suffix: process id plus a process-local counter.
fn temp_suffix() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::process::id() as u64 ^ (COUNTER.fetch_add(1, Ordering::Relaxed) << 32)
}

/// One known-repo registry entry.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Registry file path `<home>/state/v1/registry.json`.
fn registry_path(home: &Path) -> PathBuf {
    home.join("state").join("v1").join("registry.json")
}

/// Reads the known-repo registry; empty when nothing is recorded.
///
/// An absent file is not an error. An unreadable file fails with `Io`; invalid
/// JSON fails with `CorruptRecord`.
pub fn read_registry(home: &Path) -> Result<Vec<KnownRepo>, StoreError> {
    let path = registry_path(home);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(StoreError::new(
                StoreErrorCode::Io,
                format!("read {}: {e}", path.display()),
            ));
        }
    };
    serde_json::from_slice(&bytes).map_err(|e| {
        StoreError::new(
            StoreErrorCode::CorruptRecord,
            format!("invalid registry JSON: {e}"),
        )
    })
}

/// Adds or refreshes one registry entry; idempotent for identical identities.
///
/// An existing entry with the same `repo_id` is replaced, so re-registering
/// never duplicates. Entries are kept sorted by `repo_id` and written with the
/// same atomic replacement as records.
pub fn add_known_repo(home: &Path, entry: &KnownRepo) -> Result<(), StoreError> {
    let mut repos = read_registry(home)?;
    match repos
        .iter_mut()
        .find(|known| known.repo_id == entry.repo_id)
    {
        Some(existing) => *existing = entry.clone(),
        None => repos.push(entry.clone()),
    }
    repos.sort_by(|a, b| a.repo_id.cmp(&b.repo_id));
    let bytes = serde_json::to_vec(&repos)
        .map_err(|e| StoreError::new(StoreErrorCode::Io, format!("serialize registry: {e}")))?;
    atomic_write(&registry_path(home), &bytes)
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
/// Registry entries are reported first; scanning then covers each configured
/// root and its descendants to depth at most 2, never follows symlinks, skips
/// `target/` and `node_modules/` and never enters hidden directories (`.git` is
/// only used to detect repositories). A directory counts as a repository when
/// it contains a `.git` directory; linked-worktree `.git` pointer files are not
/// resolved here, because resolving them needs Git. Results stop at `budget`
/// and report `budget_exhausted`; repositories outside the scanned scope stay
/// unknown, not absent.
pub fn discover(layout: &Layout, budget: &Budget) -> Result<DiscoveryReport, StoreError> {
    let mut report = DiscoveryReport::default();
    let mut seen: Vec<PathBuf> = Vec::new();
    for entry in read_registry(&layout.home)? {
        let common_dir = canonicalize_or(&entry.common_dir);
        if seen.contains(&common_dir) {
            continue;
        }
        seen.push(common_dir.clone());
        report.repos.push(DiscoveredRepo {
            common_dir,
            source: DiscoverySource::Registry,
        });
    }
    for root in &layout.config.discovery_roots {
        scan_root(root, budget, &mut report, &mut seen);
    }
    Ok(report)
}

/// Scans one root and its descendants to depth ≤ 2 under the budget.
fn scan_root(root: &Path, budget: &Budget, report: &mut DiscoveryReport, seen: &mut Vec<PathBuf>) {
    record_repo(root, report, seen);
    let mut level: Vec<PathBuf> = vec![root.to_path_buf()];
    for _depth in 1..=2 {
        let mut next_level = Vec::new();
        for dir in &level {
            let entries = match fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(_) => {
                    report.skipped.push(dir.clone());
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        report.skipped.push(dir.clone());
                        break;
                    }
                };
                if Instant::now() >= budget.deadline || report.scanned_entries >= budget.max_entries
                {
                    report.budget_exhausted = true;
                    return;
                }
                report.scanned_entries += 1;
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == "target" || name == "node_modules" {
                    report.skipped.push(path);
                    continue;
                }
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(_) => {
                        report.skipped.push(path);
                        continue;
                    }
                };
                if file_type.is_symlink() {
                    report.skipped.push(path);
                    continue;
                }
                if !file_type.is_dir() {
                    continue;
                }
                if name.starts_with('.') {
                    // Hidden directories are never entered; `.git` is only
                    // consulted for repository detection in `record_repo`.
                    continue;
                }
                record_repo(&path, report, seen);
                next_level.push(path);
            }
        }
        level = next_level;
    }
}

/// Records `dir` when it contains a `.git` directory, deduplicating by the
/// canonicalized common directory.
fn record_repo(dir: &Path, report: &mut DiscoveryReport, seen: &mut Vec<PathBuf>) {
    let dot_git = dir.join(".git");
    if !fs::symlink_metadata(&dot_git).is_ok_and(|meta| meta.is_dir()) {
        return;
    }
    let Ok(common_dir) = fs::canonicalize(&dot_git) else {
        return;
    };
    if seen.contains(&common_dir) {
        return;
    }
    seen.push(common_dir.clone());
    report.repos.push(DiscoveredRepo {
        common_dir,
        source: DiscoverySource::Root,
    });
}

/// Canonicalizes a path when it exists; otherwise returns it unchanged.
fn canonicalize_or(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Test assertions")]
mod tests {
    use super::*;

    /// A generous budget for filesystem tests.
    fn budget(max_entries: usize) -> Budget {
        Budget {
            deadline: Instant::now() + Duration::from_secs(10),
            max_output_bytes: 4096,
            max_entries,
        }
    }

    /// A stored-record fixture for repository `a…a` and worktree `task-1`.
    fn sample_record() -> Record {
        Record {
            schema_version: RECORD_SCHEMA_VERSION,
            repo_id: "a".repeat(64),
            name: "task-1".to_owned(),
            path: PathBuf::from("/w/task-1"),
            branch: Some("aw/task-1".to_owned()),
            base_ref: None,
            base_oid: None,
            created_at: 1,
            creator: "harness".to_owned(),
            revision: 0,
            removal_started: None,
        }
    }

    /// A registry-entry fixture.
    fn known_repo(repo_id: &str, common_dir: &Path) -> KnownRepo {
        KnownRepo {
            repo_id: repo_id.to_owned(),
            common_dir: common_dir.to_path_buf(),
            label: "repo".to_owned(),
            integration_ref: "main".to_owned(),
            registered_at: 1,
        }
    }

    #[test]
    fn layout_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let platform = tmp.path().join("platform-home");
        let env_home = tmp.path().join("env-home");

        // No config anywhere: env home wins and the root defaults under it.
        let layout = resolve_layout(Some(env_home.clone()), None, platform.clone()).unwrap();
        assert_eq!(layout.home, env_home);
        assert_eq!(layout.root, env_home.join("worktrees"));
        assert_eq!(layout.config, Config::default());

        // config.toml supplies the root when no env override exists.
        fs::create_dir_all(&platform).unwrap();
        fs::write(
            platform.join("config.toml"),
            "[storage]\nroot = \"/cfg/root\"\n",
        )
        .unwrap();
        let layout = resolve_layout(None, None, platform.clone()).unwrap();
        assert_eq!(layout.root, PathBuf::from("/cfg/root"));

        // The env root beats config.toml.
        let layout =
            resolve_layout(None, Some(PathBuf::from("/env/root")), platform.clone()).unwrap();
        assert_eq!(layout.root, PathBuf::from("/env/root"));

        // The env home beats the platform home even when it holds a config.
        let layout = resolve_layout(Some(env_home.clone()), None, platform).unwrap();
        assert_eq!(layout.home, env_home);
        assert_eq!(layout.root, env_home.join("worktrees"));
    }

    #[test]
    fn invalid_config_key_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        for text in [
            "[storage]\nunknown = 1\n",
            "top_level = true\n",
            "[discovery]\nroots = \"/not/a/list\"\n",
        ] {
            fs::write(home.join("config.toml"), text).unwrap();
            let code = resolve_layout(None, None, home.to_path_buf())
                .unwrap_err()
                .code;
            assert_eq!(code, StoreErrorCode::InvalidConfig, "config {text}");
        }
    }

    #[test]
    fn record_round_trip_bumps_revision() {
        let home = tempfile::tempdir().unwrap();
        let record = sample_record();
        assert!(
            read_record(home.path(), &record.repo_id, &record.name)
                .unwrap()
                .is_none()
        );
        let stored = replace_record(home.path(), &record, 0).unwrap();
        assert_eq!(stored.revision, 1);
        assert_eq!(
            read_record(home.path(), &record.repo_id, &record.name).unwrap(),
            Some(stored.clone())
        );
        let stored = replace_record(home.path(), &stored, 1).unwrap();
        assert_eq!(stored.revision, 2);
    }

    #[test]
    fn revision_conflicts_refuse_writes() {
        let home = tempfile::tempdir().unwrap();
        let record = sample_record();
        // Nothing stored yet, but the caller claims to have observed revision 5.
        assert_eq!(
            replace_record(home.path(), &record, 5).unwrap_err().code,
            StoreErrorCode::RevisionConflict
        );
        let stored = replace_record(home.path(), &record, 0).unwrap();
        // Stale observed revision.
        assert_eq!(
            replace_record(home.path(), &stored, 0).unwrap_err().code,
            StoreErrorCode::RevisionConflict
        );
    }

    #[test]
    fn oversized_record_refused() {
        let home = tempfile::tempdir().unwrap();
        let mut record = sample_record();
        record.creator = "x".repeat(9_000);
        assert_eq!(
            replace_record(home.path(), &record, 0).unwrap_err().code,
            StoreErrorCode::RecordTooLarge
        );
        assert!(
            read_record(home.path(), &record.repo_id, &record.name)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn corrupt_record_detected() {
        let home = tempfile::tempdir().unwrap();
        let repo_id = "a".repeat(64);
        let path = home
            .path()
            .join("state/v1/repos")
            .join(&repo_id)
            .join("task-1.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            read_record(home.path(), &repo_id, "task-1")
                .unwrap_err()
                .code,
            StoreErrorCode::CorruptRecord
        );
    }

    #[test]
    fn schema_mismatch_detected() {
        let home = tempfile::tempdir().unwrap();
        let repo_id = "a".repeat(64);
        let path = home
            .path()
            .join("state/v1/repos")
            .join(&repo_id)
            .join("task-1.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let future = serde_json::json!({
            "schema_version": 99,
            "repo_id": repo_id,
            "name": "task-1",
            "path": "/w/task-1",
            "branch": null,
            "base_ref": null,
            "base_oid": null,
            "created_at": 1,
            "creator": "harness",
            "revision": 1,
        });
        fs::write(&path, future.to_string()).unwrap();
        assert_eq!(
            read_record(home.path(), &repo_id, "task-1")
                .unwrap_err()
                .code,
            StoreErrorCode::SchemaIncompatible
        );
    }

    #[test]
    fn mark_removal_started_marks_and_bumps() {
        let home = tempfile::tempdir().unwrap();
        let record = sample_record();
        let stored = replace_record(home.path(), &record, 0).unwrap();
        let fingerprint = Fingerprint::parse(&"b".repeat(64)).unwrap();
        let marked = mark_removal_started(
            home.path(),
            &record.repo_id,
            &record.name,
            &fingerprint,
            stored.revision,
            42,
        )
        .unwrap();
        assert_eq!(marked.revision, 2);
        assert_eq!(
            marked.removal_started,
            Some(RemovalStarted {
                fingerprint,
                at: 42
            })
        );
        // A missing record refuses instead of creating one.
        assert_eq!(
            mark_removal_started(
                home.path(),
                &"c".repeat(64),
                "ghost",
                &Fingerprint::parse(&"b".repeat(64)).unwrap(),
                0,
                42
            )
            .unwrap_err()
            .code,
            StoreErrorCode::RevisionConflict
        );
    }

    #[test]
    fn registry_round_trip_and_dedupe() {
        let home = tempfile::tempdir().unwrap();
        add_known_repo(
            home.path(),
            &known_repo(&"a".repeat(64), Path::new("/repo-a/.git")),
        )
        .unwrap();
        add_known_repo(
            home.path(),
            &known_repo(&"b".repeat(64), Path::new("/repo-b/.git")),
        )
        .unwrap();
        // Re-adding the same identity refreshes rather than duplicates.
        add_known_repo(
            home.path(),
            &known_repo(&"a".repeat(64), Path::new("/moved-a/.git")),
        )
        .unwrap();
        let repos = read_registry(home.path()).unwrap();
        assert_eq!(repos.len(), 2);
        assert!(
            repos
                .iter()
                .any(|repo| repo.common_dir == Path::new("/moved-a/.git"))
        );
        assert_eq!(
            read_registry(&home.path().join("missing")).unwrap(),
            Vec::new()
        );
    }

    #[tokio::test]
    async fn lock_contention_times_out_while_first_held() {
        let home = tempfile::tempdir().unwrap();
        let repo_id = "a".repeat(64);
        let first = lock_repo(home.path(), &repo_id, &budget(100))
            .await
            .unwrap();
        // A second acquisition under a tiny deadline times out.
        let tiny = Budget {
            deadline: Instant::now() + Duration::from_millis(50),
            max_output_bytes: 4096,
            max_entries: 100,
        };
        assert_eq!(
            lock_repo(home.path(), &repo_id, &tiny)
                .await
                .unwrap_err()
                .code,
            StoreErrorCode::LockTimeout
        );
        // A clone keeps the lock alive after the original guard drops.
        let shared = first.clone();
        drop(first);
        assert_eq!(
            lock_repo(home.path(), &repo_id, &tiny)
                .await
                .unwrap_err()
                .code,
            StoreErrorCode::LockTimeout
        );
        // With every guard gone the lock is free again.
        drop(shared);
        lock_repo(home.path(), &repo_id, &budget(100))
            .await
            .unwrap();
    }

    #[test]
    fn discovery_depth_skip_symlink_and_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let scan = tmp.path().join("scan");
        for dir in [
            "repoA/.git",
            "deep/repoB/.git",
            "too/deep/repoC/.git",
            "target/skip/.git",
            "node_modules/pkg/.git",
            ".hidden/.git",
        ] {
            fs::create_dir_all(scan.join(dir)).unwrap();
        }
        fs::create_dir_all(tmp.path().join("linked/.git")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("linked"), scan.join("link")).unwrap();
        fs::write(scan.join("README"), b"plain file").unwrap();
        let layout = Layout {
            home: home.clone(),
            root: home.join("worktrees"),
            config: Config {
                root: None,
                discovery_roots: vec![scan.clone()],
            },
        };
        let report = discover(&layout, &budget(10_000)).unwrap();
        let found: Vec<PathBuf> = report.repos.iter().map(|r| r.common_dir.clone()).collect();
        assert!(found.contains(&fs::canonicalize(scan.join("repoA/.git")).unwrap()));
        assert!(found.contains(&fs::canonicalize(scan.join("deep/repoB/.git")).unwrap()));
        assert!(!found.iter().any(|p| p.ends_with("repoC/.git")));
        assert!(!found.iter().any(|p| p.ends_with("skip/.git")));
        assert!(!found.iter().any(|p| p.ends_with("pkg/.git")));
        assert!(!found.iter().any(|p| p.ends_with("hidden/.git")));
        assert!(!found.iter().any(|p| p.ends_with("linked/.git")));
        assert!(report.skipped.contains(&scan.join("target")));
        assert!(report.skipped.contains(&scan.join("node_modules")));
        assert!(report.skipped.contains(&scan.join("link")));
        assert!(!report.budget_exhausted);

        // A tiny entry budget stops the scan and reports exhaustion.
        let tight = discover(
            &layout,
            &Budget {
                max_entries: 2,
                ..budget(10_000)
            },
        )
        .unwrap();
        assert!(tight.budget_exhausted);
        assert!(tight.scanned_entries <= 2);
    }

    #[test]
    fn discovery_prefers_registry_and_dedupes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let scan = tmp.path().join("scan");
        fs::create_dir_all(scan.join("repoA/.git")).unwrap();
        fs::create_dir_all(scan.join("repoB/.git")).unwrap();
        let repo_a = "a".repeat(64);
        add_known_repo(
            &home,
            &known_repo(&repo_a, &fs::canonicalize(scan.join("repoA/.git")).unwrap()),
        )
        .unwrap();
        let layout = Layout {
            home,
            root: PathBuf::new(),
            config: Config {
                root: None,
                discovery_roots: vec![scan],
            },
        };
        let report = discover(&layout, &budget(10_000)).unwrap();
        assert_eq!(report.repos.len(), 2);
        let registered = report
            .repos
            .iter()
            .find(|repo| repo.common_dir.ends_with("repoA/.git"))
            .unwrap();
        assert_eq!(registered.source, DiscoverySource::Registry);
        assert!(
            report
                .repos
                .iter()
                .any(|repo| repo.source == DiscoverySource::Root
                    && repo.common_dir.ends_with("repoB/.git"))
        );
    }
}
