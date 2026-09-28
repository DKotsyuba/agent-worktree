//! Idle-worktree notification hook (`agent-worktree hook context`).
//!
//! When a linked worktree has had no activity for over [`Policy`'s idle
//! threshold][crate::worktree::Policy] (24 h by default), one bounded
//! `<agent-worktree>…</agent-worktree>` block is injected into the
//! orchestrator's context — once per idle episode, never on every prompt. The
//! orchestrator decides what to do; this hook never mutates repositories.
//!
//! Host-safety rules for a `UserPromptSubmit` hook: the command always exits
//! 0, prints nothing on any internal error (one bounded line goes to
//! `<home>/logs/hook.log` instead), and finishes within a 3 s total deadline.
//! Scans are rate-limited to one per 10 minutes.

use crate::service::{IdleRow, IdleScan, Service};
use crate::store;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Minimum seconds between two scans; a prompt inside the window prints nothing.
const SCAN_INTERVAL_SECS: u64 = 600;
/// Total deadline for one hook run, including the scan and the state write.
const HOOK_DEADLINE: Duration = Duration::from_secs(3);
/// The scan stops this early so the state write, printing and exit still fit
/// the total deadline. Discovery over big roots runs inside this budget; an
/// over-run yields no answer (see [`scan`]) rather than a partial one.
const SCAN_MARGIN: Duration = Duration::from_millis(2800);
/// Lock wait before a concurrent hook run gives up silently.
const LOCK_WAIT: Duration = Duration::from_millis(300);
/// Hard line cap for one injected block; extra rows collapse into `+N more`.
const MAX_BLOCK_LINES: usize = 12;
/// Hard byte cap for one injected block.
const MAX_BLOCK_BYTES: usize = 1536;
/// Rows listed before the `+N more` line.
const MAX_LISTED_ROWS: usize = 10;
/// Upper bound for the append-only hook error log before it is halved.
const MAX_LOG_BYTES: u64 = 64 * 1024;
/// State file schema version.
const NOTIFY_SCHEMA_VERSION: u32 = 1;
/// Environment markers agent-run sets on launched child processes (its
/// `worker::ENV_NAMES`); any of them means this hook runs inside a delegated
/// agent whose orchestrator already owns worktree hygiene, so nothing prints.
const DELEGATE_MARKERS: [&str; 4] = [
    "AGENT_RUN_WORKER_HOME",
    "AGENT_RUN_WORKER_RUN_ID",
    "AGENT_RUN_WORKER_ATTEMPT_ID",
    "AGENT_RUN_WORKER_TOKEN",
];

/// One recorded idle episode: a canonical worktree path plus the newest
/// activity timestamp of the idle stretch that produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    /// Canonical absolute worktree path.
    pub path: PathBuf,
    /// Unix seconds of the newest cheap activity signal in this episode.
    pub last_activity: u64,
}

/// On-disk notification state at `<home>/state/v1/notify.json`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyState {
    /// Schema version of the state file.
    schema_version: u32,
    /// Unix seconds of the last completed scan.
    last_scan_at: u64,
    /// Idle episodes already notified.
    episodes: Vec<Episode>,
}

impl Default for NotifyState {
    fn default() -> Self {
        Self {
            schema_version: NOTIFY_SCHEMA_VERSION,
            last_scan_at: 0,
            episodes: Vec::new(),
        }
    }
}

/// Runs one `hook context` pass and returns the context text to inject.
///
/// `host` labels the calling harness in the error log; both hosts currently
/// receive the same `additionalContext` envelope (see `main`). An empty
/// return means "print nothing". This function never fails and never exceeds
/// [`HOOK_DEADLINE`].
pub async fn hook_context(host: &str) -> String {
    let start = Instant::now();
    match tokio::time::timeout(HOOK_DEADLINE, scan(start, host)).await {
        Ok(text) => text,
        Err(_) => {
            log_error(home_dir().as_deref(), host, "deadline_exceeded");
            String::new()
        }
    }
}

/// One full rate-limited scan pass; every failure is silent.
async fn scan(start: Instant, host: &str) -> String {
    if is_delegate_child() {
        return String::new();
    }
    let Some(home) = home_dir() else {
        return String::new();
    };
    let mut state = read_state(&home);
    let now = unix_now();
    if rate_limited(state.last_scan_at, now) {
        return String::new();
    }
    // A second hook running concurrently scans; this one stays silent.
    let Some(_lock) = try_lock(&home) else {
        return String::new();
    };
    // Re-read under the lock: the other run may have stamped a fresh scan.
    state = read_state(&home);
    if rate_limited(state.last_scan_at, now) {
        return String::new();
    }
    let Ok(service) = Service::new() else {
        log_error(Some(&home), host, "policy_invalid");
        return String::new();
    };
    let Some(scan) = service.idle_worktrees(start + SCAN_MARGIN).await else {
        // Incomplete pass (deadline or scope failure): no state change at all,
        // so neither the rate limit nor the episode set is stamped by a scan
        // that saw only part of the scope; the next prompt rescans.
        return String::new();
    };
    let episodes = episodes_from(&scan.rows);
    let fresh = new_episodes(&state.episodes, &episodes);
    // Record first: an episode that cannot be recorded must not be announced,
    // or it would repeat on every prompt once the state becomes writable.
    // Recorded episodes are never dropped on transient evidence — see
    // [`retained_episodes`].
    let next = NotifyState {
        schema_version: NOTIFY_SCHEMA_VERSION,
        last_scan_at: now,
        episodes: retained_episodes(&state.episodes, &episodes, &scan),
    };
    if let Err(error) = write_state(&home, &next) {
        log_error(Some(&home), host, &format!("write_state: {error}"));
        return String::new();
    }
    if fresh.is_empty() {
        return String::new();
    }
    let fresh_rows: Vec<IdleRow> = scan
        .rows
        .iter()
        .filter(|row| fresh.iter().any(|e| e.path == canonical(&row.path)))
        .cloned()
        .collect();
    render_block(&fresh_rows)
}

/// True when any agent-run delegate marker is present in the environment.
fn is_delegate_child() -> bool {
    DELEGATE_MARKERS
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

/// Product home directory from the environment, when it resolves.
fn home_dir() -> Option<PathBuf> {
    let env_home = std::env::var_os("AGENT_WORKTREE_HOME").map(PathBuf::from);
    let platform_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    store::resolve_layout(env_home, platform_home)
        .ok()
        .map(|layout| layout.home)
}

/// Reads the notification state; an absent, corrupt or future-schema file
/// yields the default (one extra scan, never a lost notification).
fn read_state(home: &Path) -> NotifyState {
    let path = state_path(home);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return NotifyState::default();
    };
    match serde_json::from_str(&text) {
        Ok(state) => state,
        Err(error) => {
            log_error(Some(home), "", &format!("state_corrupt: {error}"));
            NotifyState::default()
        }
    }
}

/// Atomically writes the notification state without fsync.
///
/// The file is a dedupe cache: losing it to a crash costs one repeated block,
/// never a lost notification, so the durability syncs are skipped and the
/// whole tail (serialize, temp write, rename) stays microseconds inside the
/// 3 s hook deadline instead of blocking past it on a slow disk.
fn write_state(home: &Path, state: &NotifyState) -> Result<(), String> {
    let bytes = serde_json::to_vec(state).map_err(|e| e.to_string())?;
    store::atomic_write_unsynced(&state_path(home), &bytes).map_err(|e| e.to_string())
}

/// State file path `<home>/state/v1/notify.json`.
fn state_path(home: &Path) -> PathBuf {
    home.join("state/v1/notify.json")
}

/// Acquires the exclusive notification lock, or `None` within [`LOCK_WAIT`].
///
/// Contention (`WouldBlock`) is expected and stays silent; any other lock
/// failure — a real filesystem error, not a busy peer — is logged so a broken
/// setup is attributable in `hook.log`.
fn try_lock(home: &Path) -> Option<File> {
    use fs2::FileExt;
    let path = home.join("state/v1/notify.lock");
    std::fs::create_dir_all(path.parent()?).ok()?;
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .ok()?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                log_error(Some(home), "", &format!("notify_lock_failed: {error}"));
                return None;
            }
        }
    }
}

/// True when the last scan is too recent for another one.
fn rate_limited(last_scan_at: u64, now: u64) -> bool {
    // A future stamp (clock moved back) is expired, not eternally fresh.
    last_scan_at <= now && now - last_scan_at < SCAN_INTERVAL_SECS
}

/// Builds the sorted idle-episode list from scan rows; paths are canonical so
/// symlinked spellings of one worktree share an episode.
fn episodes_from(rows: &[IdleRow]) -> Vec<Episode> {
    let mut episodes: Vec<Episode> = rows
        .iter()
        .map(|row| Episode {
            path: canonical(&row.path),
            last_activity: row.last_activity,
        })
        .collect();
    episodes.sort_by(|a, b| (&a.path, a.last_activity).cmp(&(&b.path, b.last_activity)));
    episodes.dedup();
    episodes
}

/// Canonical form of `path` when it exists on disk, else `path` unchanged.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Returns the episodes in `idle` that are new relative to `known`.
///
/// A path is announced only when its last-activity timestamp moved past the
/// recorded one: the same idle stretch never notifies twice, while a later
/// burst of activity starts a fresh episode that notifies again. A timestamp
/// that only shrank (reflog expire, gc, restored mtimes) is still the same
/// episode and stays silent.
fn new_episodes(known: &[Episode], idle: &[Episode]) -> Vec<Episode> {
    idle.iter()
        .filter(|episode| {
            known
                .iter()
                .find(|recorded| recorded.path == episode.path)
                .is_none_or(|recorded| episode.last_activity > recorded.last_activity)
        })
        .cloned()
        .collect()
}

/// Merges recorded episodes into a completed scan's fresh idle set.
///
/// A recorded episode is dropped only on positive evidence: this scan saw a
/// newer activity timestamp on the same path (the episode is over and its
/// timestamp can never recur), or every repository was inventoried and none
/// lists the registration any more (the worktree is gone). Anything weaker —
/// a failed inventory ([`IdleScan::incomplete`]), a failed observation, a
/// missing signal — is "unknown, keep": the entry survives so the next scan
/// cannot re-announce it.
fn retained_episodes(known: &[Episode], idle: &[Episode], scan: &IdleScan) -> Vec<Episode> {
    // A shrunk timestamp is the same episode: the recorded (newer) entry
    // stands in for the fresh one, so the stretch cannot re-announce once a
    // later signal grows past the shrunk value.
    let mut merged: Vec<Episode> = idle
        .iter()
        .map(|fresh| {
            known
                .iter()
                .find(|recorded| {
                    recorded.path == fresh.path && recorded.last_activity >= fresh.last_activity
                })
                .cloned()
                .unwrap_or_else(|| fresh.clone())
        })
        .collect();
    for episode in known {
        if idle.iter().any(|fresh| fresh.path == episode.path) {
            // This scan's episode for the same path supersedes the record.
            continue;
        }
        if scan
            .activity
            .get(&episode.path)
            .is_some_and(|at| *at > episode.last_activity)
        {
            // Activity moved past the recorded timestamp: episode over.
            continue;
        }
        if !scan.incomplete && !scan.registered.contains(&episode.path) {
            // Every repository inventoried, none lists it: gone.
            continue;
        }
        merged.push(episode.clone());
    }
    merged.sort_by(|a, b| (&a.path, a.last_activity).cmp(&(&b.path, b.last_activity)));
    merged.dedup();
    merged
}

/// Renders the bounded `<agent-worktree>` block for the new idle `rows`.
///
/// At most [`MAX_LISTED_ROWS`] rows are listed and anything beyond collapses
/// into one `+N more` line; the whole block is then squeezed under the
/// [`MAX_BLOCK_LINES`] line and [`MAX_BLOCK_BYTES`] byte caps, which win over
/// the row cap (very long paths drop rows into `+N more`).
fn render_block(rows: &[IdleRow]) -> String {
    let mut sorted: Vec<&IdleRow> = rows.iter().collect();
    sorted.sort_by(|a, b| (&a.label, &a.name, &a.path).cmp(&(&b.label, &b.name, &b.path)));
    let mut shown = sorted.len().min(MAX_LISTED_ROWS);
    let mut text = assemble(&sorted, shown);
    while (text.lines().count() > MAX_BLOCK_LINES || text.len() > MAX_BLOCK_BYTES) && shown > 0 {
        shown -= 1;
        text = assemble(&sorted, shown);
    }
    text
}

/// Assembles the block with the first `shown` rows.
fn assemble(rows: &[&IdleRow], shown: usize) -> String {
    let mut lines = Vec::with_capacity(MAX_BLOCK_LINES + 2);
    lines.push("<agent-worktree>".to_owned());
    lines.push(format!(
        "{} worktree(s) have had no activity for over 24 h:",
        rows.len()
    ));
    for row in &rows[..shown] {
        lines.push(format!(
            "- {}/{} — idle {} d, {}, {}",
            row.label,
            row.name,
            row.idle_days,
            row.merged,
            row.path.display()
        ));
    }
    if shown < rows.len() {
        lines.push(format!("+{} more", rows.len() - shown));
    }
    lines.push("Review: list_worktrees; remove: remove_worktree preview → apply.".to_owned());
    lines.push("</agent-worktree>".to_owned());
    lines.join("\n")
}

/// Current unix time in seconds, saturating at 0.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Appends one bounded error line to `<home>/logs/hook.log`, halving the file
/// first when it exceeds [`MAX_LOG_BYTES`]; logging failures stay silent.
/// `host` names the calling harness so a broken setup is attributable.
fn log_error(home: Option<&Path>, host: &str, context: &str) {
    let Some(home) = home else { return };
    let bounded: String = context.chars().take(200).collect();
    let dir = home.join("logs");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let log = dir.join("hook.log");
    if let Ok(meta) = std::fs::metadata(&log)
        && meta.len() > MAX_LOG_BYTES
        && let Ok(bytes) = std::fs::read(&log)
        && bytes.len() > 1
    {
        // Keep the newest half: bounded, but recent errors survive.
        let _ = std::fs::write(&log, &bytes[bytes.len() / 2..]);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&log) {
        let _ = writeln!(file, "{} hook context [{host}]: {bounded}", unix_now());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "Test assertions")]
    use super::*;

    fn row(label: &str, name: &str, days: u64) -> IdleRow {
        IdleRow {
            label: label.to_owned(),
            name: name.to_owned(),
            path: PathBuf::from(format!("/wt/{name}")),
            idle_days: days,
            last_activity: 1_000_000,
            merged: "merged",
        }
    }

    fn episode(name: &str, at: u64) -> Episode {
        Episode {
            path: PathBuf::from(format!("/wt/{name}")),
            last_activity: at,
        }
    }

    #[test]
    fn first_crossing_notifies_and_second_scan_is_silent() {
        let known: Vec<Episode> = Vec::new();
        let idle = vec![episode("task-1", 100)];
        assert_eq!(new_episodes(&known, &idle), idle);
        // Same episode again: nothing new.
        assert!(new_episodes(&idle, &idle).is_empty());
    }

    #[test]
    fn reactivity_then_idle_starts_a_new_episode() {
        let first = vec![episode("task-1", 100)];
        // Activity moved the timestamp: a new episode notifies again.
        let second = vec![episode("task-1", 500)];
        assert_eq!(new_episodes(&first, &second), second);
        // A different worktree is its own episode.
        let other = vec![episode("task-2", 100)];
        assert_eq!(new_episodes(&first, &other), other);
    }

    #[test]
    fn a_shrunk_timestamp_stays_the_same_episode() {
        let known = vec![episode("task-1", 500)];
        // The newest signal got older (reflog expire, gc, restored mtimes):
        // the same idle stretch, no second announcement.
        let shrink = vec![episode("task-1", 300)];
        assert!(new_episodes(&known, &shrink).is_empty());
        assert_eq!(retained_episodes(&known, &shrink, &empty_scan()), known);
        // Growing back past the recorded timestamp is the next episode.
        let fresh = vec![episode("task-1", 501)];
        assert_eq!(new_episodes(&known, &fresh), fresh);
    }

    #[test]
    fn rate_limit_window_is_ten_minutes() {
        let now = 10_000;
        assert!(!rate_limited(now - 600, now));
        assert!(!rate_limited(0, now));
        assert!(rate_limited(now - 599, now));
        assert!(rate_limited(now, now));
        // A future stamp is expired, not eternally fresh.
        assert!(!rate_limited(now + 60, now));
    }

    /// A scan that saw nothing of one repo (failed inventory, failed observe
    /// or missing signals) keeps every recorded episode.
    fn empty_scan() -> IdleScan {
        IdleScan::default()
    }

    #[test]
    fn recorded_episodes_survive_incomplete_scans() {
        let known = vec![episode("task-1", 100)];
        // Complete pass, registration absent everywhere: gone, dropped.
        let mut scan = empty_scan();
        scan.incomplete = false;
        assert!(retained_episodes(&known, &[], &scan).is_empty());
        // One repo failed: unknown, keep.
        let mut scan = empty_scan();
        scan.incomplete = true;
        assert_eq!(retained_episodes(&known, &[], &scan), known);
        // Registration still listed: the worktree exists, keep.
        let mut scan = empty_scan();
        scan.registered.insert(PathBuf::from("/wt/task-1"));
        assert_eq!(retained_episodes(&known, &[], &scan), known);
    }

    #[test]
    fn newer_activity_closes_and_replaces_an_episode() {
        let known = vec![episode("task-1", 100)];
        // Observed activity moved: the recorded episode is over, dropped.
        let mut scan = empty_scan();
        scan.activity.insert(PathBuf::from("/wt/task-1"), 500);
        assert!(retained_episodes(&known, &[], &scan).is_empty());
        // A fresh idle episode for the same path supersedes the record.
        let scan = empty_scan();
        let fresh = vec![episode("task-1", 900)];
        assert_eq!(retained_episodes(&known, &fresh, &scan), fresh);
    }

    #[test]
    fn block_stays_within_line_and_byte_caps() {
        let rows: Vec<IdleRow> = (0..40)
            .map(|i| row("repo", &format!("t-{i:02}"), i))
            .collect();
        let text = render_block(&rows);
        assert!(text.lines().count() <= MAX_BLOCK_LINES, "{text}");
        assert!(text.len() <= MAX_BLOCK_BYTES);
        assert!(text.starts_with("<agent-worktree>\n40 worktree(s) have had"));
        assert!(text.contains("+33 more"));
        assert!(text.ends_with(
            "Review: list_worktrees; remove: remove_worktree preview → apply.\n</agent-worktree>"
        ));
        assert_eq!(text.lines().filter(|l| l.starts_with("- ")).count(), 7);
    }

    #[test]
    fn block_lists_small_sets_without_a_more_line() {
        let rows = vec![row("repo", "task-1", 2), row("repo", "task-2", 9)];
        let text = render_block(&rows);
        assert_eq!(
            text,
            "<agent-worktree>\n2 worktree(s) have had no activity for over 24 h:\n\
             - repo/task-1 — idle 2 d, merged, /wt/task-1\n\
             - repo/task-2 — idle 9 d, merged, /wt/task-2\n\
             Review: list_worktrees; remove: remove_worktree preview → apply.\n\
             </agent-worktree>"
        );
    }
}
