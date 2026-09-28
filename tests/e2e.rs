//! End-to-end tool lifecycle over stdio with real repositories.
//!
//! Each test spawns the real binary with an isolated `AGENT_WORKTREE_HOME` and
//! `AGENT_WORKTREE_ROOT` in a temp directory and drives it through the pinned
//! SDK, so every reply is exactly what a host would see.
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

/// Spawns the server with an isolated product home and worktree root.
async fn spawn(
    home: &std::path::Path,
) -> rmcp::service::RunningService<rmcp::service::RoleClient, ()> {
    let mut command = tokio::process::Command::new(binary());
    command
        .arg("mcp")
        .env_clear()
        .env("HOME", home)
        .env("AGENT_WORKTREE_HOME", home.join("product"))
        .env("AGENT_WORKTREE_ROOT", home.join("wt-root"))
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
                "session": "sess-7", "purpose": "ship the release", "ttl": 86400}),
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
                "session": "sess-7", "purpose": "ship the release", "ttl": 86400}),
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
        assert!(text.contains("Hygiene: "), "{text}");
        // The main worktree appears as foreign.
        assert!(text.contains("| foreign |"), "{text}");

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
        assert!(text.contains("session=sess-7 ttl=86400s"), "{text}");
        let _ = path;

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
        assert!(
            receipt.contains("retained (refs/heads/aw/task-1)"),
            "{receipt}"
        );
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
        assert!(text.contains("Hygiene: missing=1"), "{text}");
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
