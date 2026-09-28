# list_worktrees

Effect: read. Response class: page (8 KiB cap, ≤ 20 rows). Idempotent: yes.

Lists Git worktree registrations across the scope with ownership
classification: `managed` (recorded by this product), `foreign` (registered
with Git, no record), `missing` (registered path absent) and
`orphan_candidate` (directory under the managed root without a registration —
never automatically deletable). Inventory only: no status or size walks unless
`size=true`, which measures only the rows shown on the page.

## Arguments

| Field | Required | Notes |
|---|---|---|
| `repo` | no | restrict the scope to one repository; omit for all known repos |
| `discovery` | no | also scan configured roots; default `true` |
| `cursor` | no | keyset cursor from the previous page |
| `limit` | no | 1–20 rows; default 20 |
| `size` | no | measure on-disk size of the page rows; default `false` |

## Pagination

Rows are stably sorted by (repository identity, path). The cursor is
`base64url("awlist1" ‖ scope digest ‖ last repo id ‖ last path)`; no snapshot
is stored. A cursor from a different scope (changed repo filter, discovery or
size flags, or a different resolved repository set) is refused with
`cursor_scope_mismatch`. Because the cursor is pure keyset, rows changed after
it was issued are seen on the next page by key, not skipped; rows whose
identity moved may appear again or drop out — there is no stored snapshot to
invalidate, and the response never claims a stable total.

## Reply

`OK` (or `PARTIAL` when one repository's inventory failed — the reply stays a
successful read and names the failure in `Coverage:`), one line per row
(`key | class | branch | creator | size | path`; creator is `-` for foreign and
missing rows), one
hygiene line `missing=… orphan_candidates=… removal_started=…` computed only
from data this call already collected, a `Coverage:` line, and `Cursor:` when
more rows remain. A page that cannot fit a single row refuses with
`response_too_large` instead of truncating an identifier; a page that would
exceed the budget is shrunk, which is safe under keyset order.

## Refusals (`ERROR …`)

- `invalid_arguments`, `limit_out_of_range`, `cursor_invalid`,
  `cursor_scope_mismatch`, `repo_path_invalid`, `not_a_repository`.
