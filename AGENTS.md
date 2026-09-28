# agent-worktree — agent instructions

Read docs/MCP_RESPONSE_STANDARD.md before touching tool responses. The product
is a stdio MCP server managing Git worktrees; docs/architecture.md is the
behaviour truth source.

## Single workflow

`cargo xtask check` is the full non-mutating gate; run it before claiming done.
Add a tool with `cargo xtask add-tool NAME --effect read|external-write
--response entity|page|ack`: it generates the typed skeleton in `src/tools/`,
wires the registry in `src/tools/mod.rs` (`definitions`, `templates`,
`incomplete`, `call`) and the template in `assets/mcp/tools/`. Generated stubs
stay unimplemented and release-blocking until their behavior and tests are
reviewed. `schemas/tools.json` is an exported snapshot, never hand-edited:
after any tool definition change run `cargo xtask contract update` and review
the diff.

## Module map

| Files | Scope |
|---|---|
| `src/worktree.rs` | shared types, pure policy: classification, removal assessment, fingerprint, validation |
| `src/git.rs` | Git subprocess access: inventory, observation, create, remove, prune, live processes, size |
| `src/store.rs` | layout/config resolution, per-repo lock, revision-checked record writes, known-repo registry, discovery |
| `src/service.rs` | orchestration: budgets, policy, locking, store + git composition |
| `src/tools/*`, `src/response.rs`, `src/main.rs` | MCP tool surface, rendering, wiring |

## Safety invariants

- Never pass `--force` to Git; never delete branches. Removal deletes the
  worktree directory only, after preview → fingerprint → apply revalidation.
- Unknown is never clean: any probe that is unknown, unavailable or incomplete
  blocks a mutation; missing signals never count as proof of abandonment.
- Every Git invocation is bounded by a deadline and output cap, runs NUL-parsed
  with `core.hooksPath` and `core.fsmonitor` suppressed and
  `GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0`.
- Responses stay within the byte budgets of docs/MCP_RESPONSE_STANDARD.md;
  rendering failure never changes the reported outcome.
- stdout of `mcp` is protocol-only; secrets never enter responses or
  diagnostics.

## Invariants and delivery

Rust 2024, resolver 3, pinned toolchain, committed application lock,
publish=false, shared lints. No Python/Node scripting dependencies, no daemon
or database. Normal MCP text uses strict embedded MiniJinja over small typed
views; no raw JSON dumping.

The profile is in-process + local state. Release is disabled and packaging
refuses non-`none` state, so install only from a local build; a stateful
delivery profile is a separate planned task. Package locally only after
committing source. Release prepare defaults to preview and never pushes. The
publisher checks qualification, tag/source/run identity, cargo-deny, no stubs
and the actual payload. Never flip qualification to make a pipeline green. No
pruning, service restart or host registration is implicit. Do not claim
native/host tests passed without evidence from those runs.
