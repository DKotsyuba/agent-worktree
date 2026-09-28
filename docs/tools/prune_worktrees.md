# prune_worktrees

Effect: external-write (when `dry_run=false`). Response class: page (8 KiB cap,
≤ 20 rows). Idempotent: no.

Prunes stale worktree registrations for one repository. Pruning removes
registrations only — never branches, never existing directories. Because
native `git worktree prune` has no exact-entry transaction, apply rechecks
repository accessibility and eligibility at execution time.

## Arguments

| Field | Required | Notes |
|---|---|---|
| `repo` | yes | repository root or any worktree inside it |
| `dry_run` | no | default `true`; lists candidates only |

## Reply

`OK prune_worktrees <id12>: N registrations; applied=false` (dry run) or
`COMMITTED … applied=true`, followed by one candidate path per row. More than
20 candidates refuses the listing (`Listing: refused …`) while still reporting
the count — rows are never silently skipped. A rendering failure after an
applied prune still emits a Rust-side `COMMITTED` receipt.

## Refusals (`ERROR …`)

- `invalid_arguments`, `repo_path_invalid`, `not_a_repository`.
- A dry-run failure is a read-style error; `OUTCOME_UNKNOWN` names the common
  directory only when an applied prune loses
  confirmation; reconcile before retrying.
- After an applied prune, records bound to pruned paths are deleted under the
  repository lock; failures surface as `record_cleanup_failed` warnings.
