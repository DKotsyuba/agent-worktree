# agent-worktree

Rust MCP generated with init.sh. Repository: DKotsyuba/agent-worktree.
Read AGENTS.md and docs/MCP_RESPONSE_STANDARD.md before adding tools.

## Usage

A stdio MCP server that keeps Git worktrees tidy for an orchestrating agent:
bounded inventory and inspection, explicit creation, and preview → apply
removal that never loses work. Git stays authoritative; local state only adds
what Git cannot reconstruct.

```bash
cargo run --locked -- mcp     # stdio endpoint; add to any MCP host
```

Tools (details and refusal codes in `docs/tools/`):

| Tool | Effect | Purpose |
|---|---|---|
| `get_status` | read | product identity and qualification status |
| `create_worktree` | external-write | create under `<root>/<label>--<id12>/<name>`, idempotent replays |
| `list_worktrees` | read | keyset-paginated inventory with classification and hygiene |
| `inspect_worktree` | read | bounded evidence: activity, integration, status, processes, size |
| `remove_worktree` | external-write | preview (fingerprint + vetoes) then apply; never `--force`, branch retained |
| `prune_worktrees` | external-write | registration-only prune, dry run by default |

Locations: product home `~/.agent-worktree/` (override `AGENT_WORKTREE_HOME`),
worktree root `<home>/worktrees` (override `AGENT_WORKTREE_ROOT` or
`config.toml`). To share the root with a harness's own worktree feature, see
`docs/harness-setup.md`. Git is never fetched from, never forced, and never
loses a branch; deletion is permanent by design.

## Development

```bash
cargo fetch --locked
cargo xtask check
cargo xtask add-tool list_items --effect read --response page
cargo xtask test protocol
cargo xtask test presentation
cargo xtask test delivery
cargo run --locked -- doctor --json
cargo run --locked -- mcp
```

Product source is in src/, the authoritative tool registry in src/tools/mod.rs,
text templates in assets/mcp/, and the exported discovery snapshot in schemas/tools.json.
`cargo xtask contract update` is an explicit reviewed snapshot change.
Generated stubs do not perform effects or return fake success; finish their contract and tests.

Cargo.lock must be reviewed and committed. A .initializing marker means initialization
was not verified; after resolving failures run cargo xtask check and remove that marker explicitly.

## Local delivery

Commit the generated source first. No remote repository is required to build a local package.

```bash
cargo xtask package
cargo xtask package verify dist/agent-worktree-0.1.0-aarch64-apple-darwin
```

The bundle executable accepts `self-install --bundle PATH --home ABS_PATH --bin-dir ABS_PATH`.
Create the bin directory explicitly. Installation preserves immutable versions, never restarts
services and does not edit MCP host configuration. `releases use VERSION` verifies and selects
a retained compatible version. This starter has no local data migration; external effects are
not undone by a binary rollback. No automatic pruning or removal of user data.

The release workflow is guarded by release.enabled=false and qualification settings in family.toml.
Version preparation is `cargo xtask release prepare VERSION --apply`; inspect/commit the changes,
then create and push an annotated tag yourself. Publishing tests the exact shipped executable.

## Template updates

`cargo xtask template diff --from /trusted/template/checkout` compares original managed bytes,
local changes and the next template. `template upgrade --dry-run` writes nothing.
Apply the reviewed plan through Git; conflicting local changes and Cargo dependencies need review.
The build has no live dependency on the private template repository.
