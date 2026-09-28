# create_worktree

Effect: external-write. Response class: entity (4 KiB cap). Idempotent: yes.

Creates one Git worktree under the managed root `<root>/<label>--<id12>/<name>/`
and records it. The default branch is `aw/<name>`; an explicit existing branch
is checked out as-is and is never reset; `detached` checks out `base` without a
branch. Creation registers the repository in the known-repo registry.

## Arguments

| Field | Required | Bounds |
|---|---|---|
| `repo` | yes | path, ≤ 1024 bytes |
| `name` | yes | 1–64 of `[a-z0-9-]`, no leading hyphen |
| `base` | no | safe refname, ≤ 200 chars; default repository HEAD |
| `branch` | no | safe refname; mutually exclusive with `detached` |
| `detached` | no | boolean |
| `creator` | yes | 1–64 printable single-line characters |
| `session` | no | 1–128 printable single-line characters |
| `purpose` | yes | 1–200 printable single-line characters |

All intake fields (`base`, `branch`, `creator`, `session`, `purpose`) are
stored in the record and compared on replay.

## Behaviour

- `COMMITTED worktree <id12>/<name>` with exact path, branch, HEAD, creator,
  purpose and creation time (UTC ISO-8601).
- Replay with the same persisted metadata: `NOOP`, reconciling against the
  original destination; nothing is recreated or overwritten.
- `base` combined with an existing `branch` is refused (Git would check the
  branch out as-is and silently drop the base).
- The first creation registers the repository, deriving its integration ref
  from the main worktree's branch; `main` is never assumed.
- A timeout or lost confirmation after the create is dispatched returns
  `OUTCOME_UNKNOWN` naming the exact destination path to inspect.

## Refusals (`ERROR …`, no effect)

- `invalid_arguments` — unknown field, missing required field, or a bound above.
- `name_length` / `name_charset` / `branch_unsafe` / `creator_invalid` /
  `session_invalid` / `purpose_invalid` / `repo_path_invalid`.
- `conflict` — destination exists without a matching record, a record exists
  with a different metadata binding, the path exists on disk, or the existing
  registration's path is gone (hint: prune first). Nothing is overwritten and
  no suffix is invented.
- `root_not_absolute` — the worktree root is not an absolute path.
- `lock_timeout` — the per-repository lock was busy past the 30 s deadline.
- `not_a_repository` — `repo` does not resolve to a Git repository.
