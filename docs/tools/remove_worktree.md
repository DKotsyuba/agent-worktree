# remove_worktree

Effect: external-write. Response class: entity for preview, ack (2 KiB) for the
apply receipt. Idempotent: no (guarded by fingerprint).

Removes one worktree, one at a time, preview → fingerprint → apply. Git is
never passed `--force` and the branch is always retained; deletion is
permanent. An interrupted apply stays visible through the record's
`removal_started` marker.

## Arguments

| Field | Required | Notes |
|---|---|---|
| `repo` | yes | repository root or any worktree inside it |
| `name` | one of name/path | worktree directory name |
| `path` | one of name/path | absolute worktree path |
| `mode` | yes | `preview` or `apply` |
| `disposable_paths` | no | ≤ 64 relative worktree-relative paths, ≤ 256 bytes each |
| `allow_unmerged` | no | boolean; explicit consent to remove an unmerged tree |
| `fingerprint` | apply | the 64-hex fingerprint returned by the preview |

## Preview

Observes everything (status, integration, processes, submodules) and returns
`PREVIEW` with the exact path, HEAD, branch (without the `refs/heads/`
prefix), approved `disposable_paths`,
`Eligible: true/false (N vetoes)` with one `Veto:` line each, warnings, and the
fingerprint. Vetoes on the preview are the requested answer, so the reply is
not an execution error; apply is only meaningful when eligible.

## Apply

Acquires the repository lock, re-observes with status checks immediately
before dispatch (Git removes an ignored-only worktree without `--force`, so the
fresh `ignored_not_disposable` veto is the only guard), reassesses with the
expected fingerprint and refuses on any veto or divergence. On success it
writes `removal_started` before dispatching `git worktree remove`, deletes the
record, then returns `COMMITTED` (or `NOOP` when already absent — including a
replayed apply whose worktree and registration are already gone; such a replay
also deletes a crashed removal's leftover `removal_started` record under the
lock — the caller path is canonicalized first, so a path-form replay through a
`/var`-style alias still matches the record's bound path — warning
`record_cleanup_pending` if that fails) with the retained branch. Foreign worktrees without records skip the marker and delete.
When the removed tree lived directly inside the configured root, apply also
removes the now-empty per-repository directory `<root>/<label>--<id12>` with a
non-recursive `remove_dir` that never touches the root itself; a failure is the
warning `repo_dir_cleanup_failed`, never an error.
The receipt is rendered from Rust if the template fails; a lost confirmation
after dispatch returns `OUTCOME_UNKNOWN` naming the exact path to inspect.

## Refusals (`ERROR …`, no effect)

- `invalid_arguments`, `invalid_mode`, `target_required`/`target_ambiguous`,
  `name_charset`, `path_invalid`, `fingerprint_required`,
  `disposable_paths_invalid`, `repo_path_invalid`.
- `removal_refused` — veto codes: `main_worktree`, `bare_repository`,
  `git_locked`, `dirty`, `untracked_files`, `conflicts`, `dirty_submodules`,
  `ignored_not_disposable` (paths), `live_process`, `fingerprint_mismatch`,
  `probe_unknown`, `unmerged` (unless `allow_unmerged`).
- `not_found`, `not_a_repository`, `lock_timeout`, `revision_conflict`.
- If Git itself refuses (for example a dirty tree), the outcome is reported,
  never forced through.
