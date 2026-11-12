# Changelog

## Unreleased

### Changed

- Gate test scratch directories are named with the PID namespace and a random suffix, and stale cleanup only removes directories of exited processes from the same namespace.
- The integration-test check ignores commented-out module lines and requires `autotests = false`.
- CI installs cargo-nextest into a runner-owned directory added to `PATH` (macOS runners lack `~/.cargo/bin`).
- `cargo xtask check` follows the Rust gate standard: tests run under cargo-nextest (required; CI installs 0.9.149 from a checksum-verified archive) with every failure reported, doctests via `cargo test --doc`, and test temporaries in a fresh tmpfs directory (`XTASK_TEST_TMPFS=0` opts out). Integration tests build into one `it` binary: every `tests/*.rs` must be a module of `tests/it/main.rs`. Release acceptance runs `--test it <suite>::` filters and refuses a filter that selects no tests. The dev profile keeps line-table debug info only (`--profile debugging` for full).

### Added

- Machine-readable read contract for `get_status`, `list_worktrees` and
  `inspect_worktree`: every reply now also carries MCP `structuredContent`
  (`{"schema_version":1,"kind":…,"data":…}`, or the same envelope with
  `{"error":{code,outcome,detail,next?}}` for any failure — a failed read is
  never an empty success) plus a matching per-tool `outputSchema` exported in
  `schemas/tools.json`. `list` rows expose the stable key, full repository
  identity (`repo_id` and the canonical Git common dir), full branch ref,
  `detached`/`head`/`creator`, `removal_started`, typed activity
  (`{state,mtime_based}`) and integration values with explicit
  unknown/unprobed `null`s, and numeric `size {bytes,quality}`;
  `inspect` keeps the five independent `Probe` states (`known`,
  `not_checked`, `unavailable`, `incomplete` with reasons), warnings as
  stable codes, the record `revision` and only actually measured timestamps.
  The compact text replies, refusals and safety behavior are unchanged; reads
  still add no records and mutate nothing.

## 0.1.3

### Fixed

- `tools/list` carries `ttlMs`/`cacheScope` for MCP 2026-07-28 clients; Claude
  Code saw zero tools. Legacy sessions are unchanged.

## 0.1.2

### Added

- `remove_worktree` batch form: `targets` (1–20 items of `name`/`path`,
  `disposable_paths`, `allow_unmerged`, `fingerprint`) removes several
  worktrees of one repository in one call. Targets are processed sequentially
  through the unchanged single-target safety path; every target gets its own
  line (`removed` / `already_absent` / `refused <codes>` /
  `outcome_unknown <path>`) plus a `removed=N refused=M unknown=K` summary
  inside the 8 KiB page budget. An oversized batch is refused as
  `batch_too_large` before any effect instead of truncated.

### Fixed

- A dispatched removal is never killed by its deadline: on timeout the Git
  process is detached (finishes in the background, reaped — no zombie) and the
  reply is `outcome_unknown` naming the path. Removals run under a documented
  10-minute per-target deadline instead of the 30-second mutation budget.
- Interrupted-removal rule: a registered worktree differing from HEAD only by
  deletions of tracked files (merged or `allow_unmerged`, no live process,
  ignored leftovers only in `disposable_paths`) no longer vetoes `dirty` when
  the interruption is evidenced — the record's own `removal_started` marker or
  the new fingerprint-bound `resume_interrupted` flag (which also covers
  foreign worktrees). The preview warns `resumed_removal` and apply restores
  exactly the observed deletion paths from the index (never the whole tree) so
  Git needs no `--force`; without evidence the `dirty` veto stands with a
  `resumable_deletion` hint naming the flag. A read-phase failure before
  dispatch (for example the pre-dispatch inventory timeout) is no longer
  reported as an unknown effect.

## 0.1.1

### Added

- Worktree creation under `<root>/<label>--<id12>/<name>` on branch `aw/<name>`
  with who/why/when metadata and idempotent replays; conflicts never overwrite.
- Keyset-paginated listing of every worktree of a known repository — managed,
  foreign (Claude Code, Codex, delegates) and missing — with activity, merge
  state and hygiene warnings for idle (≥ 24 h), stale (≥ 30 days),
  large (≥ 2 GiB) and missing worktrees.
- Bounded inspection of one worktree: classification, activity, integration
  ancestry, status, submodules, live processes, size and git lock.
- Safe removal through preview → fingerprint → apply, refusing uncommitted,
  untracked, locked, live, main-worktree and unmerged cases; Git is never
  passed `--force` and branches are always retained.
- Registration-only prune of missing worktrees, dry run by default, with
  keyset-paginated previews. There is no TTL: idle or old worktrees and state
  are never expired or removed automatically.
- Host notification hook (`agent-worktree hook context`): one context block per
  worktree idle ≥ 24 h, rate-limited, printing nothing and always exiting 0 so
  the host is never disrupted.
- Local state store: atomic revision-checked records and a known-repo registry
  under `~/.agent-worktree/state/v1/`.

### Changed

- The worktree root is configured only in `~/.agent-worktree/config.toml`
  (`[storage] root`); the `AGENT_WORKTREE_ROOT` override and the
  `<home>/worktrees` default are gone. Creation refuses `root_not_configured`
  until a root is set; listing, inspection, removal, prune and the
  notification hook work without one. A leading `~/` is accepted in
  `[storage] root` and `[discovery] roots` entries (expanded against the real
  home); relative values refuse as `invalid_config`. `doctor` prints home,
  config path, resolved root and discovery roots.

No published release is implied by the Cargo package version.
