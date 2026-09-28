# Changelog

## Unreleased

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
