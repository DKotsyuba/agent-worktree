# agent-worktree

A stdio MCP server that keeps Git worktrees tidy for an orchestrating AI agent.
Agents spin up worktrees and never clean up after themselves: copies pile up
across repositories, drift idle for weeks and quietly grow to tens of GiB.
`agent-worktree` gives the agent one standard place to put worktrees, a full
inventory of what exists (including worktrees made by Claude Code, Codex and
delegated agents), hygiene warnings for what is rotting, and a removal path
that cannot lose work. Git stays authoritative; local state only adds what Git
cannot reconstruct — who created a worktree, why, and when.

## Tools

| Tool | Effect | Purpose |
|---|---|---|
| `get_status` | read | product identity and release qualification status |
| [`create_worktree`](docs/tools/create_worktree.md) | external-write | create under `<root>/<label>--<id12>/<name>` on branch `aw/<name>`, with who/why/when metadata; idempotent replays |
| [`list_worktrees`](docs/tools/list_worktrees.md) | read | keyset-paginated inventory of every worktree of a known repository, with activity, merge state and hygiene warnings |
| [`inspect_worktree`](docs/tools/inspect_worktree.md) | read | bounded evidence for one worktree: activity, integration, status, processes, size |
| [`remove_worktree`](docs/tools/remove_worktree.md) | external-write | preview (fingerprint + vetoes) then apply; never `--force`, never deletes branches |
| [`prune_worktrees`](docs/tools/prune_worktrees.md) | external-write | drop registrations of missing worktrees; dry run by default |

Behaviour truth source: [docs/architecture.md](docs/architecture.md) and
[docs/tools/](docs/tools/). Listing warns about idle (≥ 24 h), stale
(≥ 30 days), large (≥ 2 GiB) and missing worktrees; the repository's main
checkout is listed as `main` context and never counted in those hygiene
counters. Removal is preview →
fingerprint → apply and refuses uncommitted or untracked files, ignored files
outside explicitly approved disposable paths, locked worktrees, live processes,
the main worktree, and unmerged branches without explicit consent. Pruning
removes registrations only — never branches, never existing directories.

## Build and install

Build from this checkout:

```bash
cargo build --locked        # binary at target/debug/agent-worktree
cargo build --locked --release
```

Release packaging of the local-state profile is not available yet (the release
workflow is disabled and packaging refuses non-`none` state profiles), so
installation means pointing your MCP host at a locally built binary. `doctor`
checks local readiness without side effects:

```bash
cargo run --locked -- doctor --json
```

## MCP host registration

The server speaks stdio; the subcommand is `mcp`.

Claude Code:

```sh
claude mcp add agent-worktree -- /absolute/path/to/agent-worktree mcp
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.agent-worktree]
command = "/absolute/path/to/agent-worktree"
args = ["mcp"]
```

To share the worktree root with a harness's own worktree feature, see
[docs/harness-setup.md](docs/harness-setup.md); to get one context
notification per 24 h-idle worktree, see
[docs/notifications.md](docs/notifications.md).

## Configuration

- Product home: `~/.agent-worktree/` by default, override with
  `AGENT_WORKTREE_HOME`. Contains `config.toml`, `worktrees/` and `state/`.
- Worktree root: `<home>/worktrees` by default. Precedence:
  `AGENT_WORKTREE_ROOT` > `config.toml` `[storage] root` > default.
  Changing the root affects future creation only.
- `config.toml` (at `<home>/config.toml`) accepts exactly two keys; unknown
  keys are rejected with `invalid_config`:
  - `[storage] root` — path overriding the worktree root.
  - `[discovery] roots` — list of paths scanned (depth ≤ 2) for repository
    discovery when listing across repositories.

## Development

```bash
cargo fetch --locked
cargo xtask check          # full non-mutating gate
cargo xtask test protocol
cargo xtask test presentation
cargo xtask test delivery
cargo run --locked -- mcp
```

The authoritative tool registry is Rust code in `src/tools/mod.rs`; text
templates live in `assets/mcp/`, and `schemas/tools.json` is an exported
snapshot updated explicitly with `cargo xtask contract update`. See
[AGENTS.md](AGENTS.md) for the agent contract of this repository.
