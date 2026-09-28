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
| `cursor` | no | keyset cursor from the previous preview page |
| `limit` | no | 1–20 candidate paths per preview page; default 20 |

## Reply

`OK prune_worktrees <id12>: N candidates; showing M; applied=false` (dry run)
or `COMMITTED … applied=true`, followed by one candidate path per row, a
`Cursor:` line when more preview pages remain, and warnings. The preview is
keyset-paginated over the stably sorted candidate paths — pruning has no
narrower scope, so an oversized preview pages through instead of refusing and
rows are never silently skipped. Apply ignores `cursor`/`limit` and prunes
every candidate repository-wide, reporting the full count. A cursor from a
different repository is refused with `cursor_scope_mismatch`. A rendering
failure after an applied prune still emits a Rust-side `COMMITTED` receipt.

## Refusals (`ERROR …`)

- `invalid_arguments`, `limit_out_of_range`, `cursor_invalid`,
  `cursor_scope_mismatch`, `repo_path_invalid`, `not_a_repository`.
- A dry-run failure is a read-style error; `OUTCOME_UNKNOWN` names the common
  directory only when an applied prune loses
  confirmation; reconcile before retrying.
- After an applied prune, records bound to pruned paths are deleted under the
  repository lock; failures surface as `record_cleanup_failed` warnings.
