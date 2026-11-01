# inspect_worktree

Effect: read. Response class: entity (4 KiB cap). Idempotent: yes.

Inspects one worktree with bounded probes and returns labelled evidence:
classification, path, HEAD, branch, activity, integration ancestry, status
counts, submodules, live processes, size, git lock, an optional
`removal_started` marker and one hygiene line.

## Arguments

| Field | Required | Notes |
|---|---|---|
| `repo` | yes | repository root or any worktree inside it |
| `name` | one of name/path | worktree directory name |
| `path` | one of name/path | absolute worktree path |
| `checks` | no | per-probe booleans; when omitted, status, integration and processes run |

`checks` accepts `status`, `integration`, `processes`, `submodules`, `size`.
When the object is present, every omitted probe is `false`.

## Probes

Each axis is shown as known, `not checked`, `unavailable (<code>)` or
`<value> (incomplete: <reason>)`; an unknown check is never reported as clean.
Activity defaults: recent < 24 h, idle ≥ 24 h, stale ≥ 30 d; a live process
means `active`. Integration is ancestry against the configured integration ref
(`main` until configured otherwise); squash merges are not detected.
Size is measured only when requested; budget exhaustion reports a lower bound.

## Reply

`OK worktree <id12>/<name> (<class>)` followed by labelled rows (branch without
the `refs/heads/` prefix, record creation time in UTC ISO-8601) and warnings,
`Hygiene: stale_activity=… large=… missing=…` derived from data this call
already collected. Degraded probes keep the reply a successful read.

## Refusals (`ERROR …`)

- `invalid_arguments`, `target_required`/`target_ambiguous` (name xor path),
  `name_charset`, `path_invalid`, `repo_path_invalid`.
- `not_found` — no Git registration matches; orphan directories are not
  inspectable because Git owns the registration facts.
- `not_a_repository`.
- `record_mismatch` warning when a stored record disagrees with the observed
  worktree identity or bound path; the record is ignored for that decision.

## Structured content

Every reply also carries MCP `structuredContent` conforming to the tool's
`outputSchema` (exported in `schemas/tools.json`): the outer envelope is
`{"schema_version":1,"kind":"worktree_detail","data":…}` on success and
`{"schema_version":1,"kind":"worktree_detail","error":{code,outcome,detail,next?}}`
for every failure — a failed read is never an empty success. The text block
and `isError` semantics above are unchanged, and the one text block is not a
JSON duplicate of the structured payload.

`data` keeps the independent probe axes as objects
`{state, value, code, reason}` with `state` one of `known`,
`not_checked`, `unavailable`, `incomplete` — degraded probes stay degraded
instead of collapsing into a value. It adds the canonical repository identity
(`repo_id`, `repo_common_dir`), the measured activity timestamps
(`activity_signals`, `null` where not measured), warnings as stable codes,
the record with its `revision` and `removal_started` marker, and
`observed_at`. The full branch ref is exposed; no raw config, process
arguments or credentials appear.
