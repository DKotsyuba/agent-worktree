//! End-to-end tool lifecycle over stdio with real repositories.
//!
//! These tests need the real `git` and `store` modules; until they land, every
//! test here is `#[ignore]`d and phase 2 removes the attribute. Each test uses
//! a disposable repository and an isolated `AGENT_WORKTREE_HOME`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Explicit test failures"
)]
use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use std::process::{Command, Stdio};
use std::time::Duration;

fn binary() -> std::path::PathBuf {
    std::env::var_os("MCP_TEST_BINARY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_BIN_EXE_agent-worktree")))
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.email=e2e@test", "-c", "user.name=e2e"])
        .current_dir(dir)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// Creates a committed repository with one file on `main`.
fn init_repo(dir: &std::path::Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--initial-branch=main", "--quiet"]);
    std::fs::write(dir.join("README.md"), "# repo\n").unwrap();
    git(dir, &["add", "README.md"]);
    git(dir, &["commit", "--quiet", "-m", "initial"]);
}

/// Spawns the server with an isolated product home and returns the client.
async fn spawn(
    home: &std::path::Path,
) -> rmcp::service::RunningService<rmcp::service::RoleClient, ()> {
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

#[tokio::test]
#[ignore = "needs git+store implementations"]
async fn create_list_inspect_remove_prune_lifecycle() {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("repo");
    init_repo(&repo);
    let client = spawn(home.path()).await;
    tokio::time::timeout(Duration::from_secs(60), async {
        // Create.
        let (error, text) = call_text(
            &client,
            "create_worktree",
            serde_json::json!({"repo": repo.display().to_string(), "name": "task-1",
                "creator": "e2e", "purpose": "lifecycle"}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("COMMITTED worktree "), "{text}");
        let path = text
            .lines()
            .find_map(|l| l.strip_prefix("Path: "))
            .expect("path line");
        assert!(path.contains("task-1"));

        // Idempotent replay.
        let (error, text) = call_text(
            &client,
            "create_worktree",
            serde_json::json!({"repo": repo.display().to_string(), "name": "task-1",
                "creator": "e2e", "purpose": "lifecycle"}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("NOOP worktree "), "{text}");

        // List shows a managed row.
        let (error, text) = call_text(
            &client,
            "list_worktrees",
            serde_json::json!({"repo": repo.display().to_string()}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.contains("managed"), "{text}");
        assert!(text.contains("Hygiene: "), "{text}");

        // Inspect by name.
        let (error, text) = call_text(
            &client,
            "inspect_worktree",
            serde_json::json!({"repo": repo.display().to_string(), "name": "task-1"}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("OK worktree "), "{text}");

        // Preview then apply with the returned fingerprint.
        let (error, preview) = call_text(
            &client,
            "remove_worktree",
            serde_json::json!({"repo": repo.display().to_string(), "name": "task-1",
                "mode": "preview", "disposable_paths": ["target/"]}),
        )
        .await;
        assert!(!error, "{preview}");
        let fingerprint = preview
            .lines()
            .find_map(|l| l.strip_prefix("Fingerprint: "))
            .expect("fingerprint line");
        assert_eq!(fingerprint.len(), 64);
        let (error, receipt) = call_text(
            &client,
            "remove_worktree",
            serde_json::json!({"repo": repo.display().to_string(), "name": "task-1",
                "mode": "apply", "disposable_paths": ["target/"],
                "fingerprint": fingerprint}),
        )
        .await;
        assert!(!error, "{receipt}");
        assert!(text.contains("task-1"));
        assert!(
            receipt.starts_with("COMMITTED remove_worktree "),
            "{receipt}"
        );
        assert!(!std::path::Path::new(path).exists(), "tree removed");

        // Branch is retained.
        let (error, text) = call_text(
            &client,
            "prune_worktrees",
            serde_json::json!({"repo": repo.display().to_string(), "dry_run": true}),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.starts_with("OK prune_worktrees "), "{text}");

        // Strict arguments still hold end to end.
        let (error, text) = call_text(
            &client,
            "create_worktree",
            serde_json::json!({"repo": repo.display().to_string(), "name": "x", "nope": 1}),
        )
        .await;
        assert!(error, "{text}");
        assert!(text.starts_with("ERROR invalid_arguments:"), "{text}");

        client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}

#[tokio::test]
#[ignore = "needs git+store implementations"]
async fn removal_refuses_untracked_work_and_keeps_branch() {
    let home = tempfile::tempdir().unwrap();
    let repo = home.path().join("repo");
    init_repo(&repo);
    let client = spawn(home.path()).await;
    tokio::time::timeout(Duration::from_secs(60), async {
        let (error, text) = call_text(
            &client,
            "create_worktree",
            serde_json::json!({"repo": repo.display().to_string(), "name": "wip",
                "creator": "e2e", "purpose": "refusal"}),
        )
        .await;
        assert!(!error, "{text}");
        let path = text
            .lines()
            .find_map(|l| l.strip_prefix("Path: "))
            .expect("path line");
        std::fs::write(std::path::Path::new(path).join("untracked.txt"), "x").unwrap();

        let (error, preview) = call_text(
            &client,
            "remove_worktree",
            serde_json::json!({"repo": repo.display().to_string(), "name": "wip",
                "mode": "preview"}),
        )
        .await;
        assert!(!error, "{preview}");
        assert!(preview.contains("Veto: untracked_files"), "{preview}");
        assert!(preview.contains("Eligible: false"), "{preview}");

        // Without a preview fingerprint, apply refuses outright.
        let (error, text) = call_text(
            &client,
            "remove_worktree",
            serde_json::json!({"repo": repo.display().to_string(), "name": "wip",
                "mode": "apply"}),
        )
        .await;
        assert!(error, "{text}");
        assert!(std::path::Path::new(path).exists(), "tree kept");
        client.cancel().await.unwrap();
    })
    .await
    .expect("e2e deadline");
}
