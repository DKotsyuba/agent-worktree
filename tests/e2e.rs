//! End-to-end tool lifecycle over stdio with real repositories.
//!
//! Each test spawns the real binary with an isolated `AGENT_WORKTREE_HOME`
//! (whose `config.toml` configures the worktree root) in a temp directory and
//! drives it through the pinned SDK, so every reply is exactly what a host
//! would see.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Explicit test failures"
)]
use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::time::Duration;

fn binary() -> std::path::PathBuf {
    std::env::var_os("MCP_TEST_BINARY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_BIN_EXE_agent-worktree")))
}

/// Runs a synchronous Git call for fixture setup, returning success.
fn git_status(dir: &std::path::Path, args: &[&str]) -> bool {
    Command::new("git")
        .args([
            "-c",
            "user.email=e2e@test",
            "-c",
            "user.name=e2e",
            "-c",
            "commit.gpgsign=false",
        ])
        .current_dir(dir)
        .args(args)
        .status()
        .is_ok_and(|s| s.success())
}

fn git_ok(dir: &std::path::Path, args: &[&str]) -> bool {
    git_status(dir, args)
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "user.email=e2e@test",
            "-c",
            "user.name=e2e",
            "-c",
            "commit.gpgsign=false",
        ])
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Creates a committed repository on `main` with a `.gitignore` for `target/`.
fn init_repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(dir.join("README.md"), "# repo\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "--quiet", "-m", "initial"]);
}

/// Civil (year, month, day) from days since the epoch, for `touch -t` stamps.
fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `touch -t` stamp (YYYYMMDDhhmm) for whole days before now.
fn stamp_days_ago(days: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let then = now - days * 24 * 3600;
    let (year, month, day) = civil_from_days((then / 86_400) as i64);
    let rest = then % 86_400;
    format!(
        "{year:04}{month:02}{day:02}{:02}{:02}",
        rest / 3600,
        rest / 60 % 60
    )
}

/// Ages one worktree's HEAD/index mtimes and last reflog entry to `days_ago`.
fn age_worktree(admin_dir: &std::path::Path, days_ago: u64) {
    let stamp = stamp_days_ago(days_ago);
    for file in ["HEAD", "index"] {
        Command::new("touch")
            .arg("-t")
            .arg(&stamp)
            .arg(admin_dir.join(file))
            .status()
            .unwrap();
    }
    let secs_ago = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - days_ago * 24 * 3600;
    std::fs::write(
        admin_dir.join("logs").join("HEAD"),
        format!(
            "0000000000000000000000000000000000000000 \
1111111111111111111111111111111111111111 e2e <e2e@test> {secs_ago} +0000\tageing\n"
        )
        .replace("\\n", "\n"),
    )
    .unwrap();
    Command::new("touch")
        .arg("-t")
        .arg(&stamp)
        .arg(admin_dir.join("logs").join("HEAD"))
        .status()
        .unwrap();
}

/// Finds one record file by worktree name under the product home.
fn record_file(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    let records_dir = home.join("product/state/v1/repos");
    records_dir
        .read_dir()
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .flat_map(|entry| std::fs::read_dir(entry.path()).unwrap().flatten())
        .find(|entry| entry.path().ends_with(format!("{name}.json")))
        .map(|entry| entry.path())
        .unwrap_or_else(|| panic!("record file for {name} exists"))
}

/// Spawns the server with an isolated product home whose `config.toml`
/// configures the worktree root (the only place a root can come from).
async fn spawn(
    home: &std::path::Path,
) -> rmcp::service::RunningService<rmcp::service::RoleClient, ()> {
    std::fs::create_dir_all(home.join("product")).unwrap();
    std::fs::write(
        home.join("product/config.toml"),
        format!("[storage]\nroot = \"{}\"\n", home.join("wt-root").display()),
    )
    .unwrap();
    let mut command = tokio::process::Command::new(binary());
    command
        .arg("mcp")
        .env_clear()
        .env("HOME", home)
        .env("AGENT_WORKTREE_HOME", home.join("product"))
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .current_dir(home)
        .kill_on_drop(true)
        .stderr(Stdio::null());
    ().serve(TokioChildProcess::new(command).unwrap())
        .await
        .unwrap()
}

/// Calls one tool and returns `(isError, text)`.
async fn call_text(
    client: &rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    name: &str,
    args: serde_json::Value,
) -> (bool, String) {
    let name = name.to_owned();
    let result = client
        .call_tool(
            CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    let wire = serde_json::to_value(&result).unwrap();
    let text = wire["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    (wire["isError"].as_bool().unwrap_or(true), text)
}

/// Extracts the value of one `Field: value` line.
fn field<'a>(text: &'a str, label: &str) -> &'a str {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{label}: ")))
        .unwrap_or_else(|| panic!("missing {label} line in:\n{text}"))
}

/// Runs `hook context` with an isolated product home and captures stdout.
fn hook_context(home: &std::path::Path, extra_env: &[(&str, &str)]) -> String {
    let mut command = Command::new(binary());
    command
        .args(["hook", "context", "--host", "claude"])
        .env_clear()
        .env("HOME", home)
        .env("AGENT_WORKTREE_HOME", home.join("product"))
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .current_dir(home);
    for (name, value) in extra_env {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "hook must always exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Spawns `hook context` without waiting, for concurrency tests; stdout is
/// piped so the announcement can be captured per process.
fn hook_context_spawned(home: &std::path::Path) -> std::process::Child {
    let mut command = Command::new(binary());
    command
        .args(["hook", "context", "--host", "claude"])
        .env_clear()
        .env("HOME", home)
        .env("AGENT_WORKTREE_HOME", home.join("product"))
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .current_dir(home)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    command.spawn().unwrap()
}

/// Rewrites notify.json with `last_scan_at = 0` so the next run rescans.
fn clear_last_scan(home: &std::path::Path) {
    let path = home.join("product/state/v1/notify.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    state["last_scan_at"] = serde_json::json!(0);
    std::fs::write(&path, serde_json::to_string(&state).unwrap()).unwrap();
}

/// Runs the hook until it prints. Under parallel test load a debug-build scan
/// can miss its 3 s deadline and stay silent by design, so retries with the
/// state file removed and a pause between attempts absorb that (deadline
/// misses never leave a usable scan); a hook that never prints fails here.
fn hook_context_prints(home: &std::path::Path, attempts: usize) -> String {
    for attempt in 0..attempts {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(300));
        }
        let stdout = hook_context(home, &[]);
        if !stdout.trim().is_empty() {
            return stdout;
        }
        let _ = std::fs::remove_file(home.join("product/state/v1/notify.json"));
    }
    panic!("hook context printed nothing after {attempts} attempts");
}

/// One prepared fixture: temp home, committed repo, spawned client.
struct Fixture {
    // Order matters: drop order keeps dirs alive for the whole test.
    _client: rmcp::service::RunningService<rmcp::service::RoleClient, ()>,
    home: tempfile::TempDir,
    repo: std::path::PathBuf,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("repo");
    init_repo(&repo);
    let client = spawn(home.path()).await;
    Fixture {
        _client: client,
        home,
        repo,
    }
}

#[tokio::test]
async fn create_list_inspect_lifecycle() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();
        // Create in the standard place with who/why/when.
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "creator": "claude-code",
                "session": "sess-7", "purpose": "ship the release"}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("COMMITTED worktree "), "{text}");
        assert_eq!(field(&text, "Creator"), "claude-code");
        assert_eq!(field(&text, "Purpose"), "ship the release");
        assert_eq!(field(&text, "Branch"), "aw/task-1");
        let path = field(&text, "Path").to_owned();
        assert!(
            path.contains("wt-root") && path.ends_with("/task-1"),
            "{path}"
        );
        assert!(field(&text, "Created").len() >= 10);

        // Same args again: no-op replay.
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "creator": "claude-code",
                "session": "sess-7", "purpose": "ship the release"}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("NOOP worktree "), "{text}");

        // Different purpose: conflict, never overwrite.
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "creator": "claude-code",
                "purpose": "something else"}),
        )
        .await;
        assert!(error, "{text}");
        assert!(text.starts_with("ERROR conflict:"), "{text}");

        // List shows it as managed with creator and purpose-bearing record.
        let (error, text) = call_text(
            &f._client,
            "list_worktrees",
            serde_json::json!({"repo": repo}),
        )
        .await;
        assert!(!error, "{text}");
        let row = text
            .lines()
            .find(|l| l.contains("/task-1 |"))
            .unwrap_or_else(|| panic!("task-1 row missing:\n{text}"));
        assert!(row.contains("managed"), "{row}");
        assert!(row.contains("claude-code"), "{row}");
        assert!(text.contains("Hygiene (page): "), "{text}");
        // The main worktree appears as its own context class.
        assert!(text.contains("/repo | main |"), "{text}");

        // Inspect surfaces the record line and clean probes.
        let (error, text) = call_text(
            &f._client,
            "inspect_worktree",
            serde_json::json!({"repo": repo, "name": "task-1"}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("OK worktree "), "{text}");
        assert!(text.contains("(managed)"), "{text}");
        assert!(text.contains("Record: creator=claude-code"), "{text}");
        assert!(text.contains("purpose=ship the release"), "{text}");
        assert!(text.contains("Activity: recent"), "{text}");
        assert!(text.contains("Integration: ancestor_merged"), "{text}");
        assert!(
            text.contains("Status: staged=0 unstaged=0 untracked=0"),
            "{text}"
        );
        assert!(text.contains("session=sess-7"), "{text}");
        let _ = path;

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

/// Without `[storage] root` creation refuses naming the config file, while
/// listing still works and says the orphan scan was skipped.
#[tokio::test]
async fn create_refuses_without_a_configured_root() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let f = fixture().await;
        // The service re-reads config.toml per call; drop the root.
        std::fs::write(
            f.home.path().join("product/config.toml"),
            format!("[discovery]\nroots = [\"{}\"]\n", f.home.path().display()),
        )
        .unwrap();
        let repo = f.repo.display().to_string();
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "task-9", "creator": "claude-code",
                "purpose": "prove the refusal"}),
        )
        .await;
        assert!(error, "{text}");
        assert!(
            text.starts_with(
                "ERROR root_not_configured: no worktree root is configured; set one in "
            ),
            "{text}"
        );
        assert!(
            text.contains(
                f.home
                    .path()
                    .join("product/config.toml")
                    .display()
                    .to_string()
                    .as_str()
            ),
            "{text}"
        );
        assert!(text.contains("[storage]"), "{text}");

        // Listing keeps working and names the skipped orphan scan.
        let (error, text) = call_text(
            &f._client,
            "list_worktrees",
            serde_json::json!({"repo": &repo}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(
            text.contains("worktree root not configured; orphan scan skipped"),
            "{text}"
        );

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn removal_vetoes_fingerprint_apply_and_replay() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "creator": "e2e",
                "purpose": "removal flow"}),
        )
        .await;
        assert!(!error, "{text}");
        let path = field(&text, "Path").to_owned();

        // Untracked file vetoes the preview.
        std::fs::write(std::path::Path::new(&path).join("u.txt"), "u\n").unwrap();
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(preview.contains("Veto: untracked_files"), "{preview}");
        assert!(preview.contains("Eligible: false"), "{preview}");
        std::fs::remove_file(std::path::Path::new(&path).join("u.txt")).unwrap();

        // Ignored target/ blocks unless approved as disposable.
        std::fs::create_dir_all(std::path::Path::new(&path).join("target/debug")).unwrap();
        std::fs::write(
            std::path::Path::new(&path).join("target/debug/agent"),
            "bin\n",
        )
        .unwrap();
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(
            preview.contains("Veto: ignored_not_disposable (target/)"),
            "{preview}"
        );
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "mode": "preview",
                "disposable_paths": ["target"]}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(preview.contains("Eligible: true (0 vetoes)"), "{preview}");
        let fingerprint = field(&preview, "Fingerprint").to_owned();
        assert_eq!(fingerprint.len(), 64);

        // Apply without the fingerprint refuses outright.
        let (error, text) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "mode": "apply",
                "disposable_paths": ["target"]}),
        )
        .await;
        assert!(error, "{text}");
        assert!(text.starts_with("ERROR fingerprint_required:"), "{text}");

        // A stale fingerprint (tree changed since preview) refuses.
        std::fs::write(std::path::Path::new(&path).join("u2.txt"), "u\n").unwrap();
        let (error, text) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "mode": "apply",
                "disposable_paths": ["target"], "fingerprint": fingerprint}),
        )
        .await;
        assert!(error, "{text}");
        assert!(
            text.contains("fingerprint_mismatch") || text.contains("untracked_files"),
            "{text}"
        );
        std::fs::remove_file(std::path::Path::new(&path).join("u2.txt")).unwrap();

        // Apply with the preview fingerprint: tree gone, branch kept, record gone.
        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "mode": "apply",
                "disposable_paths": ["target"], "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(
            receipt.starts_with("COMMITTED remove_worktree "),
            "{receipt}"
        );
        assert!(receipt.contains("retained (aw/task-1)"), "{receipt}");
        assert!(!std::path::Path::new(&path).exists(), "tree removed");
        assert!(git_ok(
            &f.repo,
            &["rev-parse", "--verify", "refs/heads/aw/task-1"]
        ));
        let (error, text) = call_text(
            &f._client,
            "list_worktrees",
            serde_json::json!({"repo": repo}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(!text.contains("/task-1 |"), "record cleaned:\n{text}");

        // Replaying the same apply is a no-op.
        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "mode": "apply",
                "disposable_paths": ["target"], "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(receipt.starts_with("NOOP remove_worktree "), "{receipt}");
        assert!(receipt.contains("already absent"), "{receipt}");

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn removing_the_last_worktree_cleans_the_repo_directory() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();

        // Two managed worktrees under one per-repository directory.
        let mut paths = Vec::new();
        for name in ["task-1", "task-2"] {
            let (error, text) = call_text(
                &f._client,
                "create_worktree",
                serde_json::json!({"repo": repo, "name": name, "creator": "e2e",
                    "purpose": "repo directory cleanup"}),
            )
            .await;
            assert!(!error, "{text}");
            paths.push(field(&text, "Path").to_owned());
        }
        let repo_dir = std::path::Path::new(&paths[0]).parent().unwrap().to_owned();

        // Removing one of two keeps the directory; removing the last one
        // removes the now-empty directory with it.
        for (index, name) in ["task-1", "task-2"].iter().enumerate() {
            let (error, preview) = call_text(
                &f._client,
                "remove_worktree",
                serde_json::json!({"repo": repo, "name": name, "mode": "preview"}),
            )
            .await;
            assert!(!error, "{preview}");
            assert!(preview.contains("Eligible: true (0 vetoes)"), "{preview}");
            let fingerprint = field(&preview, "Fingerprint").to_owned();
            let (error, receipt) = call_text(
                &f._client,
                "remove_worktree",
                serde_json::json!({"repo": repo, "name": name, "mode": "apply",
                    "fingerprint": fingerprint}),
            )
            .await;
            assert!(!error, "{receipt}");
            assert!(
                receipt.starts_with("COMMITTED remove_worktree "),
                "{receipt}"
            );
            // The first removal leaves the sibling tree behind: the still
            // populated directory is kept without any cleanup warning.
            assert!(
                !receipt.contains("repo_dir_cleanup_failed"),
                "iteration {index}: {receipt}"
            );
            assert!(
                !std::path::Path::new(&paths[index]).exists(),
                "tree removed"
            );
            assert_eq!(repo_dir.exists(), index == 0, "iteration {index}");
        }

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn unmerged_and_foreign_removal_rules() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "wip", "creator": "e2e",
                "purpose": "unmerged work"}),
        )
        .await;
        assert!(!error, "{text}");
        let path = field(&text, "Path").to_owned();

        // A commit main does not contain makes the tree unmerged.
        git(
            std::path::Path::new(&path),
            &["commit", "--quiet", "--allow-empty", "-m", "wip"],
        );
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "wip", "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(preview.contains("Veto: unmerged"), "{preview}");
        let fingerprint = field(&preview, "Fingerprint").to_owned();

        // Apply without the explicit flag still refuses.
        let (error, text) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "wip", "mode": "apply",
                "fingerprint": fingerprint}),
        )
        .await;
        assert!(error, "{text}");
        assert!(text.contains("unmerged"), "{text}");
        assert!(std::path::Path::new(&path).exists());

        // With the explicit flag it is removed; the unmerged branch survives.
        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "wip", "mode": "apply",
                "allow_unmerged": true, "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(
            receipt.starts_with("COMMITTED remove_worktree "),
            "{receipt}"
        );
        assert!(git_ok(
            &f.repo,
            &["rev-parse", "--verify", "refs/heads/aw/wip"]
        ));

        // A foreign worktree (plain git worktree add outside the managed root)
        // is listed as foreign and removable under the same vetoes.
        let foreign = f.home.path().join("foreign-wt");
        git(
            &f.repo,
            &["worktree", "add", "--quiet", &foreign.display().to_string()],
        );
        let (error, text) = call_text(
            &f._client,
            "list_worktrees",
            serde_json::json!({"repo": repo}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.contains("foreign-wt | foreign |"), "{text}");

        // Clean foreign tree: eligible without any record.
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "path": foreign.display().to_string(),
                "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(preview.contains("Eligible: true (0 vetoes)"), "{preview}");
        let fingerprint = field(&preview, "Fingerprint").to_owned();
        // Dirty foreign tree: refused.
        std::fs::write(foreign.join("u.txt"), "u\n").unwrap();
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "path": foreign.display().to_string(),
                "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(preview.contains("Veto: untracked_files"), "{preview}");
        std::fs::remove_file(foreign.join("u.txt")).unwrap();
        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "path": foreign.display().to_string(),
                "mode": "apply", "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(
            receipt.starts_with("COMMITTED remove_worktree "),
            "{receipt}"
        );
        assert!(!foreign.exists());

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

/// Extracts the fingerprint from one batch preview line.
fn line_fingerprint(line: &str) -> String {
    line.rsplit("fingerprint=")
        .next()
        .unwrap_or_else(|| panic!("no fingerprint in line: {line}"))
        .trim()
        .to_owned()
}

#[tokio::test]
async fn batch_removal_previews_applies_and_replays() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(90), async {
        let repo = f.repo.display().to_string();
        let mut paths = Vec::new();
        for name in ["batch-1", "batch-2", "batch-3"] {
            let (error, text) = call_text(
                &f._client,
                "create_worktree",
                serde_json::json!({"repo": repo, "name": name, "creator": "e2e",
                    "purpose": "batch removal"}),
            )
            .await;
            assert!(!error, "{text}");
            paths.push(field(&text, "Path").to_owned());
        }
        // The third tree carries an untracked file.
        std::fs::write(std::path::Path::new(&paths[2]).join("u.txt"), "u\n").unwrap();

        // Batch preview: two eligible, one vetoed by untracked_files.
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "mode": "preview", "targets": [
                {"name": "batch-1"}, {"name": "batch-2"}, {"name": "batch-3"}]}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(
            preview.starts_with("PREVIEW remove_worktree batch: 3 target(s)"),
            "{preview}"
        );
        assert!(
            preview.contains("Summary: eligible=2 refused=1"),
            "{preview}"
        );
        let eligible_count = preview
            .lines()
            .filter(|l| l.contains("eligible=true"))
            .count();
        assert_eq!(eligible_count, 2, "{preview}");
        let refused_line = preview
            .lines()
            .find(|l| l.contains("/batch-3 |"))
            .unwrap_or_else(|| panic!("batch-3 line missing:\n{preview}"));
        assert!(
            refused_line.contains("vetoes=untracked_files"),
            "{refused_line}"
        );
        let fingerprints: Vec<String> = ["batch-1", "batch-2", "batch-3"]
            .iter()
            .map(|name| {
                let line = preview
                    .lines()
                    .find(|l| l.contains(&format!("/{name} |")))
                    .unwrap_or_else(|| panic!("{name} line missing:\n{preview}"));
                line_fingerprint(line)
            })
            .collect();
        assert!(fingerprints.iter().all(|fp| fp.len() == 64));

        // Batch apply with every fingerprint: two removed, the third refused
        // and untouched.
        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "mode": "apply", "targets": [
                {"name": "batch-1", "fingerprint": fingerprints[0]},
                {"name": "batch-2", "fingerprint": fingerprints[1]},
                {"name": "batch-3", "fingerprint": fingerprints[2]}]}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(receipt.contains("/batch-1 | removed"), "{receipt}");
        assert!(receipt.contains("/batch-2 | removed"), "{receipt}");
        assert!(
            receipt.contains("/batch-3 | refused untracked_files"),
            "{receipt}"
        );
        assert!(
            receipt.contains("Summary: removed=2 refused=1 unknown=0"),
            "{receipt}"
        );
        assert!(!std::path::Path::new(&paths[0]).exists());
        assert!(!std::path::Path::new(&paths[1]).exists());
        assert!(std::path::Path::new(&paths[2]).exists(), "third stays");
        for name in ["batch-1", "batch-2", "batch-3"] {
            assert!(git_ok(
                &f.repo,
                &["rev-parse", "--verify", &format!("refs/heads/aw/{name}")]
            ));
        }

        // Replaying the same batch: two already absent, the third still refused.
        let (error, replay) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "mode": "apply", "targets": [
                {"name": "batch-1", "fingerprint": fingerprints[0]},
                {"name": "batch-2", "fingerprint": fingerprints[1]},
                {"name": "batch-3", "fingerprint": fingerprints[2]}]}),
        )
        .await;
        assert!(!error, "{replay}");
        assert!(replay.contains("/batch-1 | already_absent"), "{replay}");
        assert!(replay.contains("/batch-2 | already_absent"), "{replay}");
        assert!(
            replay.contains("/batch-3 | refused untracked_files"),
            "{replay}"
        );
        assert!(
            replay.contains("Summary: removed=0 refused=1 unknown=0"),
            "{replay}"
        );
        assert!(std::path::Path::new(&paths[2]).exists());

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn interrupted_removal_needs_evidence_or_the_explicit_flag() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(90), async {
        let repo = f.repo.display().to_string();
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "half-deleted", "creator": "e2e",
                "purpose": "interrupted removal"}),
        )
        .await;
        assert!(!error, "{text}");
        let path = field(&text, "Path").to_owned();

        // Only tracked files deleted — the interrupted-removal signature, but
        // also perfectly valid pending work.
        std::fs::remove_file(std::path::Path::new(&path).join("README.md")).unwrap();
        std::fs::remove_file(std::path::Path::new(&path).join(".gitignore")).unwrap();

        // Without evidence the tree stays dirty, and the preview names the flag.
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "half-deleted", "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(preview.contains("Eligible: false"), "{preview}");
        assert!(preview.contains("Veto: dirty"), "{preview}");
        assert!(preview.contains("resumable_deletion"), "{preview}");
        assert!(!preview.contains("resumed_removal"), "{preview}");
        let plain_fingerprint = field(&preview, "Fingerprint").to_owned();

        // Apply without the flag refuses and touches nothing.
        let (error, text) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "half-deleted", "mode": "apply",
                "fingerprint": plain_fingerprint}),
        )
        .await;
        assert!(error, "{text}");
        assert!(text.contains("removal_refused: dirty"), "{text}");
        assert!(std::path::Path::new(&path).exists());

        // The explicit flag is the evidence (and part of the fingerprint).
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "half-deleted", "mode": "preview",
                "resume_interrupted": true}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(preview.contains("Eligible: true (0 vetoes)"), "{preview}");
        assert!(preview.contains("resumed_removal"), "{preview}");
        assert_ne!(field(&preview, "Fingerprint"), plain_fingerprint);
        let flagged_fingerprint = field(&preview, "Fingerprint").to_owned();

        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "half-deleted", "mode": "apply",
                "fingerprint": flagged_fingerprint, "resume_interrupted": true}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(
            receipt.starts_with("COMMITTED remove_worktree "),
            "{receipt}"
        );
        assert!(!std::path::Path::new(&path).exists(), "tree removed");
        assert!(git_ok(
            &f.repo,
            &["rev-parse", "--verify", "refs/heads/aw/half-deleted"]
        ));

        // Our own crashed removal (removal_started record) is automatic
        // evidence: no flag needed.
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "crashed", "creator": "e2e",
                "purpose": "crashed removal"}),
        )
        .await;
        assert!(!error, "{text}");
        let crashed_path = field(&text, "Path").to_owned();
        std::fs::remove_file(std::path::Path::new(&crashed_path).join("README.md")).unwrap();
        let record_path = record_file(f.home.path(), "crashed");
        let record = std::fs::read_to_string(&record_path).unwrap();
        let marker = "\"removal_started\":{\"fingerprint\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"at\":1},\"revision\":1";
        let crashed_record = record.replace("\"revision\":1", marker);
        std::fs::write(&record_path, crashed_record).unwrap();

        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "crashed", "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(preview.contains("Eligible: true (0 vetoes)"), "{preview}");
        assert!(preview.contains("resumed_removal"), "{preview}");
        let fingerprint = field(&preview, "Fingerprint").to_owned();

        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "crashed", "mode": "apply",
                "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(
            receipt.starts_with("COMMITTED remove_worktree "),
            "{receipt}"
        );
        assert!(!std::path::Path::new(&crashed_path).exists());
        assert!(!record_path.exists(), "record cleaned up");
        assert!(git_ok(
            &f.repo,
            &["rev-parse", "--verify", "refs/heads/aw/crashed"]
        ));

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

/// Twenty veto-heavy rows fit the page whole; an argument-provably oversized
/// batch is refused with a clear code instead of a row-less fallback.
#[tokio::test]
async fn batch_preview_shows_every_veto_heavy_row_or_refuses_cleanly() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(120), async {
        let repo = f.repo.display().to_string();
        for index in 0..20 {
            let name = format!("veto-{index:02}");
            let (error, text) = call_text(
                &f._client,
                "create_worktree",
                serde_json::json!({"repo": repo, "name": name, "creator": "e2e",
                    "purpose": "veto-heavy batch"}),
            )
            .await;
            assert!(!error, "{text}");
            std::fs::write(
                std::path::Path::new(field(&text, "Path")).join("u.txt"),
                "u\n",
            )
            .unwrap();
        }
        let targets: Vec<_> = (0..20)
            .map(|index| serde_json::json!({"name": format!("veto-{index:02}")}))
            .collect();
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "mode": "preview", "targets": targets}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(
            preview.starts_with("PREVIEW remove_worktree batch: 20 target(s)"),
            "{preview}"
        );
        // Every row is present and vetoed, with no presentation degradation.
        assert_eq!(
            preview
                .lines()
                .filter(|l| l.contains("eligible=false | vetoes=untracked_files"))
                .count(),
            20,
            "{preview}"
        );
        assert!(
            preview.contains("Summary: eligible=0 refused=20"),
            "{preview}"
        );
        assert!(!preview.contains("presentation_failed"), "{preview}");
        assert!(
            preview.len() <= 8192,
            "page budget exceeded: {}",
            preview.len()
        );

        // Nine long path targets cannot fit the page: refused up front with a
        // clear code, never a truncated or row-less reply.
        let long = format!("/w/{}", "d".repeat(900));
        let oversized: Vec<_> = (0..9).map(|_| serde_json::json!({"path": &long})).collect();
        let (error, refused) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "mode": "preview", "targets": oversized}),
        )
        .await;
        assert!(error, "{refused}");
        assert!(refused.starts_with("ERROR batch_too_large:"), "{refused}");
        assert!(refused.contains("smaller batch"), "{refused}");

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn old_worktree_shows_as_stale_in_list_and_hygiene() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();
        for name in ["old-task", "two-day-task"] {
            let (error, text) = call_text(
                &f._client,
                "create_worktree",
                serde_json::json!({"repo": repo, "name": name, "creator": "e2e",
                    "purpose": "ageing"}),
            )
            .await;
            assert!(!error, "{text}");
        }

        // Age one worktree far past the 30-day stale band and one just past
        // the 24-hour idle band; classify uses the reflog entry timestamp.
        age_worktree(&f.repo.join(".git").join("worktrees").join("old-task"), 40);
        age_worktree(
            &f.repo.join(".git").join("worktrees").join("two-day-task"),
            2,
        );
        // The main checkout is aged past stale too: it stays a context row
        // and never feeds the idle/stale hygiene counters.
        age_worktree(&f.repo.join(".git"), 40);

        // The list shows the bands and counts linked worktrees only.
        let (error, text) = call_text(
            &f._client,
            "list_worktrees",
            serde_json::json!({"repo": repo}),
        )
        .await;
        assert!(!error, "{text}");
        let old = text
            .lines()
            .find(|l| l.contains("/old-task |"))
            .unwrap_or_else(|| panic!("old-task row missing:\n{text}"));
        assert!(old.contains("stale_candidate~"), "{old}");
        let idle = text
            .lines()
            .find(|l| l.contains("/two-day-task |"))
            .unwrap_or_else(|| panic!("two-day-task row missing:\n{text}"));
        assert!(idle.contains("idle_candidate~"), "{idle}");
        let main = text
            .lines()
            .find(|l| l.contains("/repo | main |"))
            .unwrap_or_else(|| panic!("main row missing:\n{text}"));
        assert!(main.contains("stale_candidate~"), "{main}");
        assert!(text.contains("idle=2 stale=1"), "{text}");
        assert!(text.contains("Legend:"), "{text}");

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn create_replay_refuses_when_registration_path_is_gone() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "creator": "e2e",
                "purpose": "gone"}),
        )
        .await;
        assert!(!error, "{text}");
        let path = field(&text, "Path").to_owned();

        // The registration survives a raw directory deletion as prunable.
        std::fs::remove_dir_all(&path).unwrap();
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "creator": "e2e",
                "purpose": "gone"}),
        )
        .await;
        assert!(error, "{text}");
        assert!(text.starts_with("ERROR conflict:"), "{text}");
        assert!(text.contains("prune_worktrees"), "{text}");

        // Pruning clears the registration. The default branch is retained by
        // design, so recovery re-creates by checking the existing branch out.
        let (error, text) = call_text(
            &f._client,
            "prune_worktrees",
            serde_json::json!({"repo": repo, "dry_run": false}),
        )
        .await;
        assert!(!error, "{text}");
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "creator": "e2e",
                "purpose": "gone", "branch": "aw/task-1"}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("COMMITTED worktree "), "{text}");

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn apply_replay_deletes_a_crashed_removals_record() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "crash-task", "creator": "e2e",
                "purpose": "crash"}),
        )
        .await;
        assert!(!error, "{text}");

        // Simulate a crashed removal: keep the record, graft a
        // removal_started marker onto it after a clean apply removed tree,
        // registration and record.
        let records_dir = f.home.path().join("product/state/v1/repos");
        let record_path = records_dir
            .read_dir()
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().is_dir())
            .flat_map(|entry| std::fs::read_dir(entry.path()).unwrap().flatten())
            .find(|entry| entry.path().ends_with("crash-task.json"))
            .map(|entry| entry.path())
            .expect("record file exists");
        let record = std::fs::read_to_string(&record_path).unwrap();
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "crash-task", "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        let fingerprint = field(&preview, "Fingerprint");
        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "crash-task", "mode": "apply",
                "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(
            receipt.starts_with("COMMITTED remove_worktree "),
            "{receipt}"
        );
        assert!(!record_path.exists(), "record deleted by apply");

        // Resurrect the record as a crashed removal would have left it.
        let crashed = record.replace(
            "\"revision\":1",
            &format!(
                "\"removal_started\":{{\"fingerprint\":\"{fingerprint}\",\"at\":1}},\"revision\":1"
            ),
        );
        std::fs::write(&record_path, crashed).unwrap();

        // The replayed apply is a no-op that also clears the stale record.
        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "crash-task", "mode": "apply",
                "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(receipt.starts_with("NOOP remove_worktree "), "{receipt}");
        assert!(!record_path.exists(), "crashed record cleaned up");

        // The name is free again; the retained branch is checked out as-is.
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "crash-task", "creator": "e2e",
                "purpose": "recreated", "branch": "aw/crash-task"}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("COMMITTED worktree "), "{text}");

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn prune_preview_applies_and_pagination_visits_every_row_once() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();
        // Three managed worktrees plus one manually deleted registration.
        for name in ["task-a", "task-b", "task-c"] {
            let (error, text) = call_text(
                &f._client,
                "create_worktree",
                serde_json::json!({"repo": repo, "name": name, "creator": "e2e",
                    "purpose": "pagination"}),
            )
            .await;
            assert!(!error, "{text}");
        }
        let (error, text) = call_text(
            &f._client,
            "list_worktrees",
            serde_json::json!({"repo": repo}),
        )
        .await;
        assert!(!error, "{text}");
        let gone = text
            .lines()
            .find(|l| l.contains("/task-a |"))
            .and_then(|row| row.rsplit(" | ").next())
            .unwrap_or_else(|| panic!("task-a row missing:\n{text}"))
            .to_owned();
        std::fs::remove_dir_all(&gone).unwrap();

        // Dry run lists the deleted worktree; apply prunes the registration.
        let (error, dry) = call_text(
            &f._client,
            "prune_worktrees",
            serde_json::json!({"repo": repo, "dry_run": true}),
        )
        .await;
        assert!(!error, "{dry}");
        assert!(dry.starts_with("OK prune_worktrees "), "{dry}");
        assert!(dry.contains(gone.as_str()), "{dry}\n(gone: {gone})");
        let (error, text) = call_text(
            &f._client,
            "list_worktrees",
            serde_json::json!({"repo": repo}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.contains("| missing |"), "{text}");
        assert!(text.contains("Hygiene (page): missing=1"), "{text}");
        let (error, applied) = call_text(
            &f._client,
            "prune_worktrees",
            serde_json::json!({"repo": repo, "dry_run": false}),
        )
        .await;
        assert!(!error, "{applied}");
        assert!(
            applied.starts_with("COMMITTED prune_worktrees "),
            "{applied}"
        );
        let (error, text) = call_text(
            &f._client,
            "list_worktrees",
            serde_json::json!({"repo": repo}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(!text.contains("| missing |"), "{text}");

        // limit=1 pagination walks every remaining row exactly once.
        let mut cursor = None;
        let mut keys = BTreeSet::new();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages <= 20, "pagination did not terminate");
            let mut args = serde_json::json!({"repo": repo, "limit": 1});
            if let Some(cursor) = &cursor {
                args["cursor"] = serde_json::json!(cursor);
            }
            let (error, text) = call_text(&f._client, "list_worktrees", args).await;
            assert!(!error, "{text}");
            for row in text.lines().skip(1).take_while(|l| l.contains(" | ")) {
                keys.insert(row.split(" | ").next().unwrap().to_owned());
            }
            match text.lines().find_map(|l| l.strip_prefix("Cursor: ")) {
                Some(next) => cursor = Some(next.to_owned()),
                None => break,
            }
        }
        // main repo row plus two surviving managed worktrees, one page each.
        assert_eq!(keys.len(), 3, "keys: {keys:?}");
        assert_eq!(pages, 3, "one row per page expected");
        assert!(
            keys.iter()
                .all(|k| k.ends_with("/task-b") || k.ends_with("/task-c") || k.ends_with("/repo")),
            "keys: {keys:?}"
        );

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn apply_replay_by_aliased_path_deletes_a_crashed_removals_record() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "alias-task", "creator": "e2e",
                "purpose": "alias replay"}),
        )
        .await;
        assert!(!error, "{text}");
        let path = field(&text, "Path").to_owned();
        let repo_dir = std::path::Path::new(&path).parent().unwrap().to_owned();

        // A second managed worktree keeps the per-repository directory alive
        // through alias-task's removal, so the aliased replay below can still
        // resolve its parent through the symlink.
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "keeper", "creator": "e2e",
                "purpose": "keeps the repo directory"}),
        )
        .await;
        assert!(!error, "{text}");
        // Simulate a crashed removal: remember the record, apply a clean
        // removal by name, then resurrect the record with a removal_started
        // marker on it.
        let record_path = record_file(f.home.path(), "alias-task");
        let record = std::fs::read_to_string(&record_path).unwrap();
        let (error, preview) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "alias-task", "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        let fingerprint = field(&preview, "Fingerprint");
        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "name": "alias-task", "mode": "apply",
                "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(!record_path.exists(), "record deleted by apply");
        let crashed = record.replace(
            "\"revision\":1",
            &format!(
                "\"removal_started\":{{\"fingerprint\":\"{fingerprint}\",\"at\":1}},\"revision\":1"
            ),
        );
        std::fs::write(&record_path, crashed).unwrap();

        // Replay the apply by path, addressed through a symlink alias of the
        // managed root: textually different from the record's bound path,
        // resolving to the same directory.
        let alias = f.home.path().join("alias-root");
        std::os::unix::fs::symlink(f.home.path().join("wt-root"), &alias).unwrap();
        let aliased = alias.join(repo_dir.file_name().unwrap()).join("alias-task");
        let (error, receipt) = call_text(
            &f._client,
            "remove_worktree",
            serde_json::json!({"repo": repo, "path": aliased.display().to_string(),
                "mode": "apply", "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(receipt.starts_with("NOOP remove_worktree "), "{receipt}");
        assert!(
            !record_path.exists(),
            "crashed record cleaned up via the aliased path"
        );

        // The name is free again; the retained branch is checked out as-is.
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "alias-task", "creator": "e2e",
                "purpose": "recreated", "branch": "aw/alias-task"}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("COMMITTED worktree "), "{text}");

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn unreadable_repo_directory_is_named_in_coverage() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let repo = f.repo.display().to_string();
        let (error, text) = call_text(
            &f._client,
            "create_worktree",
            serde_json::json!({"repo": repo, "name": "task-1", "creator": "e2e",
                "purpose": "scan"}),
        )
        .await;
        assert!(!error, "{text}");
        let repo_dir = std::path::Path::new(field(&text, "Path"))
            .parent()
            .unwrap()
            .to_owned();

        // Replace the per-repo directory with a plain file: the orphan scan
        // cannot read it and must say so instead of implying orphan=0.
        std::fs::remove_dir_all(&repo_dir).unwrap();
        std::fs::write(&repo_dir, "not a directory\n").unwrap();
        let (error, text) = call_text(
            &f._client,
            "list_worktrees",
            serde_json::json!({"repo": repo}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("PARTIAL worktrees"), "{text}");
        assert!(text.contains("orphan_scan_failed"), "{text}");

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn prune_preview_paginates_oversized_candidate_lists() {
    let f = fixture().await;
    tokio::time::timeout(Duration::from_secs(120), async {
        let repo = f.repo.display().to_string();
        let mut paths = Vec::new();
        for index in 0..22 {
            let name = format!("p-{index:02}");
            let (error, text) = call_text(
                &f._client,
                "create_worktree",
                serde_json::json!({"repo": repo, "name": name, "creator": "e2e",
                    "purpose": "pagination"}),
            )
            .await;
            assert!(!error, "{text}");
            paths.push(field(&text, "Path").to_owned());
        }
        for path in &paths {
            std::fs::remove_dir_all(path).unwrap();
        }

        // The preview pages through all 22 candidates instead of refusing.
        let (error, first) = call_text(
            &f._client,
            "prune_worktrees",
            serde_json::json!({"repo": repo, "dry_run": true}),
        )
        .await;
        assert!(!error, "{first}");
        assert!(first.starts_with("OK prune_worktrees "), "{first}");
        assert!(first.contains("22 candidates; showing 20"), "{first}");
        assert!(first.contains("Cursor: "), "{first}");
        let cursor = first
            .lines()
            .find_map(|l| l.strip_prefix("Cursor: "))
            .unwrap()
            .to_owned();
        let (error, second) = call_text(
            &f._client,
            "prune_worktrees",
            serde_json::json!({"repo": repo, "dry_run": true, "cursor": cursor}),
        )
        .await;
        assert!(!error, "{second}");
        assert!(second.contains("22 candidates; showing 2"), "{second}");
        assert!(!second.contains("Cursor: "), "{second}");

        // Apply stays repository-wide and reports the full count.
        let (error, applied) = call_text(
            &f._client,
            "prune_worktrees",
            serde_json::json!({"repo": repo, "dry_run": false}),
        )
        .await;
        assert!(!error, "{applied}");
        assert!(
            applied.starts_with("COMMITTED prune_worktrees "),
            "{applied}"
        );
        assert!(applied.contains("22 candidates; showing 22"), "{applied}");

        f._client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
async fn hook_context_notifies_once_per_idle_episode() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        init_repo(&repo);
        std::fs::create_dir_all(home.path().join("product")).unwrap();
        std::fs::write(
            home.path().join("product/config.toml"),
            format!("[discovery]\nroots = [\"{}\"]\n", home.path().display()),
        )
        .unwrap();
        let wt = home.path().join("wt-old");
        git(
            &repo,
            &["worktree", "add", "--quiet", &wt.display().to_string()],
        );
        age_worktree(&repo.join(".git").join("worktrees").join("wt-old"), 2);
        // The main checkout is even older: it must never be notified.
        age_worktree(&repo.join(".git"), 40);

        // First crossing: exactly one row, for the aged linked worktree.
        let envelope: serde_json::Value =
            serde_json::from_str(hook_context_prints(home.path(), 6).trim()).unwrap();
        assert_eq!(
            envelope["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        let text = envelope["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        let canonical = std::fs::canonicalize(&wt).unwrap();
        // On this unloaded single-repo fixture the cheap merge-base probe is
        // deterministic: wt-old sits exactly at the integration tip, so the
        // mergedness label is pinned, not accepted loosely.
        assert!(
            text.starts_with(
                "<agent-worktree>\n1 worktree(s) have had no activity for over 24 h:\n- repo/wt-old — idle 2 d, merged, "
            ),
            "{text}"
        );
        assert!(
            text.contains(canonical.display().to_string().as_str()),
            "{text}"
        );
        assert!(text.ends_with(
            "Review: list_worktrees; remove: remove_worktree preview → apply.\n</agent-worktree>"
        ));
        assert!(text.lines().count() <= 12, "{text}");
        assert!(text.len() <= 1536, "{text}");

        // The state records the episode, so a rescan stays silent…
        assert_eq!(hook_context(home.path(), &[]), "");
        // …inside the rate-limit window and after it.
        clear_last_scan(home.path());
        assert_eq!(hook_context(home.path(), &[]), "");

        // A delegated agent-run child never prints, even on a fresh window.
        clear_last_scan(home.path());
        assert_eq!(
            hook_context(home.path(), &[("AGENT_RUN_WORKER_HOME", "/run/home")]),
            ""
        );

        // Activity closes the episode; idle again opens a new one. The fresh
        // commit is off main, so the second block reports unmerged — pinned,
        // not accepted loosely, same as the first block.
        git(&wt, &["commit", "--quiet", "--allow-empty", "-m", "fresh"]);
        clear_last_scan(home.path());
        assert_eq!(hook_context(home.path(), &[]), "");
        age_worktree(&repo.join(".git").join("worktrees").join("wt-old"), 2);
        clear_last_scan(home.path());
        let envelope: serde_json::Value =
            serde_json::from_str(hook_context_prints(home.path(), 6).trim()).unwrap();
        let text = envelope["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(
            text.contains("1 worktree(s) have had no activity"),
            "{text}"
        );
        assert!(
            text.contains("- repo/wt-old — idle 2 d, unmerged, "),
            "{text}"
        );
    })
    .await
    .expect("e2e deadline");
}

/// The episode state survives a scan in which the owning repository's
/// inventory fails, and the recovered scan does not re-announce.
#[tokio::test]
async fn hook_keeps_episodes_across_a_failing_repository_scan() {
    tokio::time::timeout(Duration::from_secs(60), async {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        init_repo(&repo);
        std::fs::create_dir_all(home.path().join("product")).unwrap();
        std::fs::write(
            home.path().join("product/config.toml"),
            format!("[discovery]\nroots = [\"{}\"]\n", home.path().display()),
        )
        .unwrap();
        let wt = home.path().join("wt-old");
        git(
            &repo,
            &["worktree", "add", "--quiet", &wt.display().to_string()],
        );
        age_worktree(&repo.join(".git").join("worktrees").join("wt-old"), 2);
        age_worktree(&repo.join(".git"), 40);

        // Announce once.
        let stdout = hook_context_prints(home.path(), 6);
        assert!(stdout.contains("1 worktree(s)"), "{stdout}");

        // Break the repository: discovery still resolves the scope (`.git`
        // exists) but the inventory subprocess fails — a transient omission,
        // not a removed worktree.
        std::fs::set_permissions(repo.join(".git"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        clear_last_scan(home.path());
        assert_eq!(hook_context(home.path(), &[]), "");
        let state: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.path().join("product/state/v1/notify.json")).unwrap(),
        )
        .unwrap();
        let canonical = std::fs::canonicalize(&wt).unwrap();
        assert!(
            state["episodes"]
                .as_array()
                .is_some_and(|episodes| !episodes.is_empty()),
            "the failing scan must not drop recorded episodes: {state}"
        );
        assert_eq!(
            state["episodes"][0]["path"],
            canonical.display().to_string(),
            "{state}"
        );

        // Repository back: the still-recorded episode is not re-announced.
        std::fs::set_permissions(repo.join(".git"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        clear_last_scan(home.path());
        assert_eq!(hook_context(home.path(), &[]), "");
    })
    .await
    .expect("e2e deadline");
}

/// Two hook processes racing on one home announce exactly one block.
#[tokio::test]
async fn two_concurrent_hooks_announce_exactly_once() {
    tokio::time::timeout(Duration::from_secs(60), async {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        init_repo(&repo);
        std::fs::create_dir_all(home.path().join("product")).unwrap();
        std::fs::write(
            home.path().join("product/config.toml"),
            format!("[discovery]\nroots = [\"{}\"]\n", home.path().display()),
        )
        .unwrap();
        let wt = home.path().join("wt-old");
        git(
            &repo,
            &["worktree", "add", "--quiet", &wt.display().to_string()],
        );
        age_worktree(&repo.join(".git").join("worktrees").join("wt-old"), 2);
        age_worktree(&repo.join(".git"), 40);
        let state_path = home.path().join("product/state/v1/notify.json");

        // Two hooks start before either can stamp a scan. Under parallel
        // debug-build load both can miss the 3 s deadline and stay silent by
        // design, so rounds repeat until at least one hook in a round really
        // completed (printed or wrote state); each completed round announces
        // at most one block.
        let mut completed_rounds = 0;
        let mut announcements = 0;
        for round in 0..6 {
            if round > 0 {
                std::thread::sleep(Duration::from_millis(300));
            }
            let _ = std::fs::remove_file(&state_path);
            let first = hook_context_spawned(home.path());
            let second = hook_context_spawned(home.path());
            let mut printed = 0;
            for child in [first, second] {
                let output = child.wait_with_output().unwrap();
                assert!(output.status.success(), "hook must always exit 0");
                if !String::from_utf8_lossy(&output.stdout).trim().is_empty() {
                    printed += 1;
                }
            }
            announcements += printed;
            if printed > 0 || state_path.exists() {
                completed_rounds += 1;
                assert!(printed <= 1, "round {round} announced {printed} blocks");
                break;
            }
        }
        assert!(completed_rounds > 0, "no hook completed a scan");
        assert!(announcements > 0, "no round announced the idle worktree");
        // Exactly one episode was recorded for the idle worktree.
        if let Ok(text) = std::fs::read_to_string(&state_path)
            && let Ok(state) = serde_json::from_str::<serde_json::Value>(&text)
        {
            assert_eq!(state["episodes"].as_array().map(Vec::len), Some(1));
        }
    })
    .await
    .expect("e2e deadline");
}
