# Qualification record

Release qualification for `agent-worktree` is recorded in `family.toml`
(`qualification`, `qualified_targets`, `qualified_hosts`). This file lists the
evidence behind the current record. A passing presentation test alone is never
qualification evidence.

## Current record

- Target: `aarch64-apple-darwin` (macOS 27.0, arm64).
- Hosts: Claude Code 2.1.280, Codex CLI 0.156.1.
- Source: `main` after `e2dae10`.

## Evidence

| Area | What was run | Result |
|---|---|---|
| Native build and gate | `cargo xtask check` (fmt, clippy `-D warnings` twice, all tests twice, rustdoc, contract) locally and in CI on `macos-26` arm64 | green; CI `check` and `dependencies` (cargo deny) green on `ae59939` |
| Black-box MCP | `tests/protocol.rs` and `tests/e2e.rs` drive the built binary over stdio against real Git repositories: create, list, inspect, remove preview/apply with every veto, prune, pagination, `hook context` | 101 unit, 15 end-to-end and 3 protocol tests green |
| Delivery | `cargo xtask package` → `package verify` → `self-install` into a temporary home holding `config.toml` and `state/v1/*` → launcher `--version`, `doctor`, `releases use` | green; state and config byte-identical before and after |
| Live integration, Claude Code | MCP server connected (`claude mcp list`); a headless session called `list_worktrees` and received a correct page; the `UserPromptSubmit` hook injected one `<agent-worktree>` block in a live session | passed |
| Live integration, Codex | a headless `codex exec` session called `list_worktrees` through the configured MCP server and received a correct page | passed |
| Live use on the host | worktrees created in the configured root, listed across 40 repositories, removed through preview → fingerprint → apply (vetoes observed: live process, untracked files, ignored files), stale registrations pruned with paged preview | passed; no work or branch lost |

## Known limits

- Only macOS arm64 is qualified.
- The Codex hook path was configured but its injection was not observed live (the rate limit window was active); the Claude Code hook was observed.
- Claude Code `WorktreeCreate` hook and the Codex worktree-root setting described in `docs/harness-setup.md` are documented, not qualified.
