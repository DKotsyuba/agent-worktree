# remove_worktree

Effect: external-write. Response class: entity for preview, ack (2 KiB) for the
apply receipt, page (8 KiB) for the batch forms. Idempotent: no (guarded by
fingerprint).

Removes one worktree — or a batch of up to 20 targets of one repository —
preview → fingerprint → apply. Git is never passed `--force` and the branch is
always retained; deletion is permanent. An interrupted apply stays visible
through the record's `removal_started` marker, and a tree left half-deleted by
an interrupted removal is resumed once the interruption is evidenced (see
below).

## Arguments

Single-target form:

| Field | Required | Notes |
|---|---|---|
| `repo` | yes | repository root or any worktree inside it |
| `name` | one of name/path | worktree directory name |
| `path` | one of name/path | absolute worktree path |
| `mode` | yes | `preview` or `apply` |
| `disposable_paths` | no | ≤ 64 relative worktree-relative paths, ≤ 256 bytes each |
| `allow_unmerged` | no | boolean; explicit consent to remove an unmerged tree |
| `fingerprint` | apply | the 64-hex fingerprint returned by the preview |
| `resume_interrupted` | no | boolean; asserts this target's deletions-only changes are an interrupted removal to finish; part of the fingerprint |

Batch form (`targets`, mutually exclusive with `name`, `path`,
`disposable_paths`, `allow_unmerged`, `fingerprint` and `resume_interrupted` —
mixing them refuses `targets_conflict`):

| Field | Required | Notes |
|---|---|---|
| `repo` | yes | the one repository every target belongs to; a target registered in another repository is reported as `refused not_found` on its line |
| `mode` | yes | `preview` or `apply`, for every target |
| `targets` | yes | 1–20 items, each `{ "name"? | "path"?, "disposable_paths"?, "allow_unmerged"?, "fingerprint"?, "resume_interrupted"? }` (exactly one of name/path per item; empty or over-20 batches refuse `targets_invalid`) |

## Preview

Observes everything (status, integration, processes, submodules) and returns
`PREVIEW` with the exact path, HEAD, branch (without the `refs/heads/`
prefix), approved `disposable_paths`,
`Eligible: true/false (N vetoes)` with one `Veto:` line each, warnings, and the
fingerprint. Vetoes on the preview are the requested answer, so the reply is
not an execution error; apply is only meaningful when eligible.

A batch preview returns one compact line per target — key, path, head, branch,
`eligible`, `vetoes`, `warnings`, `fingerprint` — plus
`Summary: eligible=N refused=M`. A target that cannot be resolved shows its
stable refusal code (for example `not_found`) in the `vetoes` field. The exact
rendered size is checked against the 8 KiB page budget: a preview that cannot
fit whole is refused as `batch_too_large` (no effect happened) instead of being
truncated or degraded, and if the template itself fails, the complete page is
rendered from Rust so every row stays visible.

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

A batch apply processes targets sequentially, each through exactly the
single-target safety path (lock, fresh observation, fingerprint-guarded
assessment, dispatch, record cleanup); a refusal or failure of one target never
stops, hides or rolls back the others. The receipt carries one line per target
— `removed`, `already_absent`, `refused <veto codes>` or
`outcome_unknown <path>` — plus `Summary: removed=N refused=M unknown=K`.
Refused lines are the requested answer, so a mixed batch is not an execution
error; any `outcome_unknown` line makes the reply an error, because it demands
reconciliation. When a bound of the receipt lines computed from the arguments
cannot fit the 8 KiB page budget, the whole batch is refused as
`batch_too_large` before any effect (the bound covers the removed, absent and
refused lines; the rare `outcome_unknown` line with a long resolved path is
carried complete by the Rust-rendered receipt fallback rather than hidden).

## Deadline and never killing an in-flight removal

Every removal runs under a documented 10-minute per-target deadline
(`REMOVE_MUTATION_SECS`, covering lock wait, verification and the dispatched
Git call; in a batch the deadline applies per target). A dispatched mutation is
never killed by that deadline: on timeout the Git process is detached, finishes
in the background and is reaped, and the reply is `OUTCOME_UNKNOWN` naming the
path and saying the removal may still be running. Read-only probes keep
kill-on-timeout, and a read-phase failure before dispatch (for example the
pre-dispatch inventory) is reported as the read failure it is, never as an
unknown effect. This is what keeps a large tree from being left half-deleted
by the budget itself.

## Interrupted-removal rule

If a registered worktree differs from HEAD only through deletions of tracked
files (status v2: every entry is a worktree-side deletion; no untracked,
staged, modified, renamed or conflicted entries; ignored leftovers allowed only
within `disposable_paths` as today), HEAD is merged into the integration ref
(or `allow_unmerged` is set) and no live process occupies the tree, the tree
MAY be a half-finished removal — but deletions alone are not evidence, they are
equally valid pending work. Resuming additionally requires explicit evidence:

- the record's own `removal_started` marker (our interrupted apply), or
- the request flag `resume_interrupted: true` — needed for foreign worktrees,
  which have no record.

With evidence there is no `dirty` veto: the preview reports the warning
`resumed_removal` and apply finishes the removal, restoring exactly the
observed deletion paths from the index (never the whole tree, so a concurrent
edit cannot be reset) so Git needs no `--force`. Without evidence the `dirty`
veto stands and the preview adds the warning `resumable_deletion` naming the
flag. The flag is part of the fingerprint: a preview taken with it applies only
under the same flag. Anything else keeps today's vetoes unchanged.

## Refusals (`ERROR …`, no effect)

- `invalid_arguments`, `invalid_mode`, `target_required`/`target_ambiguous`,
  `name_charset`, `path_invalid`, `fingerprint_required`,
  `disposable_paths_invalid`, `repo_path_invalid`, `targets_conflict`,
  `targets_invalid` (empty, over 20, or an item without exactly one of
  name/path), `batch_too_large`.
- `removal_refused` — veto codes: `main_worktree`, `bare_repository`,
  `git_locked`, `dirty`, `untracked_files`, `conflicts`, `dirty_submodules`,
  `ignored_not_disposable` (paths), `live_process`, `fingerprint_mismatch`,
  `probe_unknown`, `unmerged` (unless `allow_unmerged`).
- `not_found`, `not_a_repository`, `lock_timeout`, `revision_conflict`.
- If Git itself refuses (for example a dirty tree), the outcome is reported,
  never forced through.
