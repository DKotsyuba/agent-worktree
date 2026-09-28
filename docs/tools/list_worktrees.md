# list_worktrees

Effect: read. Response class: page (8 KiB cap, ≤ 20 rows). Idempotent: yes.

Lists worktrees across the scope with ownership classification: `main` (the
repository's main checkout — context only), `managed` (recorded by this
product), `foreign` (registered with Git, no record), `missing` (registered
path absent) and `orphan_candidate` (directory under the managed root without
a registration — never automatically deletable).

Every existing row on a page also carries cheap signals: an mtime-based
activity band and mergedness. The activity band comes from the HEAD/index
mtimes and the last reflog entry timestamp (never lsof, never a status walk)
and is marked with `~` (`recent~`, `idle_candidate~`, `stale_candidate~`) so it
is never mistaken for process-verified activity; mergedness is one bounded
`merge-base --is-ancestor` per row against the derived integration ref. Rows
whose pass did not finish under the page budget show `unknown`, never a guess;
missing and orphan rows show `-`. `size=true` additionally measures the rows
shown on the page.

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
(`key | class | branch | creator | activity | integration | size | path`;
branch drops the `refs/heads/` prefix; creator is `-` for main, foreign and
missing rows, size `-` unless requested), a hygiene line over exactly the rows
this page collected (`missing=… idle=… stale=… unmerged=…` plus
`orphan_candidates=` and `removal_started=` when non-zero and `large=` when
requested; idle includes stale),
a `Legend:` line, a `Coverage:` line, and `Cursor:` when more rows remain. Main
checkouts are listed but excluded from the idle, stale, unmerged and large
hygiene counters — those describe cleanup candidates, and a repository's main
checkout is never one. The whole call shares one deadline; repositories not
reached are named in `Coverage:` as `deadline_exceeded`, a per-repo directory
that could not be read is named as `orphan_scan_failed` (so `orphan=0` is
never shown for an unread directory), and a truncated orphan scan is flagged.
The encoded cursor is charged against the same row budget: a page whose
cursor would overflow the 8 KiB cap is shrunk (or refused for a single
oversized row) instead of falling back. A
page that cannot fit a single row refuses with `response_too_large` instead of
truncating an identifier; a page that would exceed the budget is shrunk, which
is safe under keyset order.

## Refusals (`ERROR …`)

- `invalid_arguments`, `limit_out_of_range`, `cursor_invalid`,
  `cursor_scope_mismatch`, `repo_path_invalid`, `not_a_repository`,
  `root_not_absolute` (relative `AGENT_WORKTREE_ROOT` or `[storage] root`).
