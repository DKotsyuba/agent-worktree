//! Black-box tests against a spawned binary using the pinned SDK's lifecycle.
//! This is SDK-pair compatibility, not certification of all external agent hosts.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Explicit test failures"
)]
use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use std::{
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
fn binary() -> PathBuf {
    std::env::var_os("MCP_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_agent-worktree")))
}
#[tokio::test]
async fn protocol() {
    let root = tempfile::tempdir().unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut command = tokio::process::Command::new(binary());
        command
            .arg("mcp")
            .env_clear()
            .env("HOME", root.path())
            .env(
                "PATH",
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )
            .current_dir(root.path())
            .kill_on_drop(true)
            .stderr(Stdio::null());
        let client = ().serve(TokioChildProcess::new(command).unwrap()).await.unwrap();
        let listed = client.list_tools(Default::default()).await.unwrap();
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/tools.json")).unwrap();
        assert_eq!(serde_json::to_value(&listed.tools).unwrap(), expected);
        assert_eq!(listed.tools.len(), 6);
        let result = client
            .call_tool(CallToolRequestParams::new("get_status"))
            .await
            .unwrap();
        let wire = serde_json::to_value(result).unwrap();
        assert_eq!(wire["isError"], false);
        assert_eq!(wire["content"].as_array().unwrap().len(), 1);
        // The tool surface works end to end: an empty scope lists zero rows.
        let listed_page = client
            .call_tool(CallToolRequestParams::new("list_worktrees"))
            .await
            .unwrap();
        let wire = serde_json::to_value(listed_page).unwrap();
        assert_eq!(wire["isError"], false);
        let text = wire["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("OK worktrees: 0 returned; more=false"),
            "{text}"
        );
        let bad_tool_args = serde_json::json!({"repo":"/repo","limit":0})
            .as_object()
            .unwrap()
            .clone();
        let refused = client
            .call_tool(CallToolRequestParams::new("list_worktrees").with_arguments(bad_tool_args))
            .await
            .unwrap();
        let wire = serde_json::to_value(refused).unwrap();
        assert_eq!(wire["isError"], true);
        let text = wire["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("ERROR limit_out_of_range:"), "{text}");
        let args = serde_json::json!({"unexpected":true})
            .as_object()
            .unwrap()
            .clone();
        let invalid = client
            .call_tool(CallToolRequestParams::new("get_status").with_arguments(args))
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(invalid).unwrap()["isError"], true);
        assert!(
            client
                .call_tool(CallToolRequestParams::new("missing_tool"))
                .await
                .is_err()
        );
        client.cancel().await.unwrap();
    })
    .await
    .expect("protocol deadline");
}
#[test]
fn eof_releases_stdio_process() {
    let mut child = std::process::Command::new(binary())
        .arg("mcp")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("MCP process survived stdin EOF");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
