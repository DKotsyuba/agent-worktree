# Changelog

## [Unreleased]

### Added

- Worktree creation under `<root>/<label>--<id12>/<name>` on branch `aw/<name>`
  with who/why/when metadata and idempotent replays; conflicts never overwrite.
- Keyset-paginated listing of every worktree of a known repository — managed,
  foreign (Claude Code, Codex, delegates) and missing — with activity, merge
  state and hygiene warnings for idle (≥ 7 days), stale (≥ 30 days),
  large (≥ 2 GiB) and missing worktrees.
- Bounded inspection of one worktree: classification, activity, integration
  ancestry, status, submodules, live processes, size and git lock.
- Safe removal through preview → fingerprint → apply, refusing uncommitted,
  untracked, locked, live, main-worktree and unmerged cases; Git is never
  passed `--force` and branches are always retained.
- Registration-only prune of missing worktrees, dry run by default.
- Local state store: atomic revision-checked records and a known-repo registry
  under `~/.agent-worktree/state/v1/`.

No published release is implied by the Cargo package version.
