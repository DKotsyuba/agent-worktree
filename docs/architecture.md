# Architecture

Profile: in-process + local state, stdio MCP, no host adapter, release disabled.

`agent-worktree` manages Git worktrees for an orchestrator: bounded inventory and
inspection, explicit creation, and preview → apply removal that never loses work.
It is one Rust binary; there is no daemon, database, watcher, relocation or
network client. Git stays authoritative for existence and branch association;
product state only adds what cannot be reconstructed from Git.

## Locations, identity and naming

- Product home: `~/.agent-worktree/`, overridable with `AGENT_WORKTREE_HOME`.
- Worktree root: `<home>/worktrees`; precedence `AGENT_WORKTREE_ROOT` >
  `config.toml` (`[storage] root`) > default. Changing the root affects future
  creation only.
- Repository identity: SHA-256 of the canonical absolute Git common directory,
  hashed with a versioned encoding (`aw-repo-id-v1`). Linked worktrees share one
  identity; independent clones remain distinct. Moving a repository changes its
  identity and requires explicit rebinding.
- Per-repository directory: `<root>/<label>--<id12>/`, where `label` is bounded
  ASCII (1–32 of `[a-z0-9.-]`, no leading hyphen or dot) and `id12` is the first
  12 identity characters. Identity never depends on the label.
- Worktree directory: `<root>/<label>--<id12>/<name>/`. The name is
  caller-supplied: 1–64 characters of `[a-z0-9-]`, no leading hyphen.
- Default branch: `aw/<name>`. An explicit existing branch or a detached checkout
  is supported; an existing branch is never reset to satisfy creation.
- Any existing directory, branch conflict, or different metadata binding is a
  `conflict`. Nothing is overwritten and no suffix is invented; a repeated
  request reconciles against the original destination.

## State (local)

Records are small JSON files under `<home>/state/v1/repos/<repo-id>/`, separate
from working files and Git administration directories.

Worktree record, schema v1 (`src/worktree.rs` `Record`, `deny_unknown_fields`):
`schema_version`, `repo_id`, `name`, bound `path`, `branch`, optional
`base_ref`/`base_oid`, `created_at`, `creator` (attribution, not
authentication), `revision`, and optional `removal_started {fingerprint, at}`.

- Writes are atomic replacements capped at 8 KiB.
- A per-repository advisory file lock (via `fs2`) serializes cooperating
  CLI/MCP processes; it cannot stop a foreign process.
- Every write is revision-checked: the caller states the observed `revision`,
  the write stores `revision + 1`, and a moved revision is a conflict.
- `removal_started` is written before a removal is dispatched, so an
  interrupted removal stays visible after a restart.
- A known-repo registry in the same directory maps identity → common dir, label
  and integration ref; `create` records repositories there.
- There is no operation journal, no stored removal plans, no pagination
  snapshots and no SQLite. Replays are decided from Git plus the record.

## Discovery

Repositories come from the known-repo registry plus configured roots
(`[discovery] roots`) scanned to depth ≤ 2 under an explicit entry/deadline
budget, skipping `target/` and `node_modules/` and never following symlinks.
Results are deduplicated by common directory. Worktrees classify as `managed`
(created through this product), `foreign` (registered with Git, no record),
`missing` (registered path absent) or `orphan_candidate` (directory without an
accessible registration). Orphan candidates are never automatically deletable,
and repositories outside the scanned scope remain unknown, not absent.

## Observation

Each worktree is described on independent axes, each carried as
`Probe<T> = Known(T) | NotChecked | Unavailable(code) | Incomplete { evidence,
reason }`, so an unknown check is never mistaken for a clean result.

| Axis | Values | Notes |
|---|---|---|
| activity | `active`, `recent`, `idle_candidate`, `stale_candidate`, `unknown` | live process ⇒ active; defaults recent ≤ 24 h, idle ≥ 7 d, stale ≥ 30 d |
| integration | `ancestor_merged`, `unmerged`, `unknown` | ancestry against the configured integration ref only; no squash detection |
| protection | main/bare, git-locked, live process cwd, dirty/untracked/conflicts/submodules, ignored outside approved | any hit blocks removal |
| size | `complete`, `lower_bound`, `not_checked` | warning at ≥ 2 GiB |

Thresholds live in `Policy` (with its own `revision`, part of the removal
fingerprint) and must be ordered `recent_after_secs` ≤ `idle_after_secs` ≤
`stale_after_secs`; `Policy::validate` rejects inverted thresholds. Age up to
`recent_after_secs` is `recent`; the gap between "recent" and "idle" also stays
`recent` — claiming activity longer is the conservative direction; missing
signals yield `unknown`, never proof of abandonment. A signal timestamped in
the future counts as age zero (clock-skew tolerance).

Signals, strongest first:

1. A visible process with cwd equal to or beneath the worktree (bounded `lsof`
   pass over current-user cwd data, no command lines; permission failures mean
   incomplete coverage).
2. HEAD/index mtimes and the timestamp inside the latest HEAD reflog entry (not
   the reflog file's mtime, not commit-author date).
3. Dirty/untracked state is a removal risk, not proof of recent use.

## Size

Measured only on request, never to decorate a mutation receipt. Traversal is
budgeted by deadline and entry count, does not follow symlinks or cross
filesystems implicitly, counts allocated bytes and avoids double-counting hard
links. Budget exhaustion yields a lower bound, never a fabricated complete
total.

## Removal

Removal is preview → fingerprint → apply, one worktree at a time.

The preview returns the exact path, HEAD, branch, observed risks, the proposed
discarded ignored paths (`disposable_paths`, explicit and relative to the
worktree — `target/` is not implicitly disposable), and the fingerprint. No
plan is stored; the fingerprint is the plan.

`fingerprint` is a SHA-256 over a versioned, length-prefixed encoding of:
canonical path, HEAD, branch, the digest of `git status --porcelain=v2 -z
--ignored=matching`, the normalized (sorted, deduplicated) `disposable_paths`,
the record revision, and the policy revision.

Apply recomputes everything under the repository lock, writes
`removal_started` into the record, and refuses on any divergence:

- main worktree or bare repository root;
- git-locked worktree (`git worktree lock` is the pin mechanism);
- dirty tracked state, untracked files, conflicts, dirty or unsupported submodules;
- ignored files outside the approved `disposable_paths`;
- a live process inside the tree, or insufficient/timeout process coverage;
- any required probe unknown, unavailable or incomplete;
- fingerprint mismatch (anything moved between preview and apply);
- unmerged HEAD, unless the request sets `allow_unmerged` explicitly — by owner
  decision unmerged copies are not removed by default, and an explicit
  confirmation keeps agents from reaching for `--force`.

Git is never passed `--force`, and the branch is always retained (commits are
never lost). Deletion is permanent; quarantine/trash was rejected because copies
with build caches can weigh tens of GiB. If Git itself refuses (for example a
dirty tree), the outcome is reported, never forced through.

## Pruning

`prune_worktrees` is repository-wide with `{repo, dry_run}`. Dry run lists
missing, unlocked registrations; apply rechecks repository accessibility and
eligibility, then runs native `git worktree prune`, which removes registrations
only — never branches or existing directories. Because native prune has no
exact-entry transaction, apply authorizes the eligible set at execution time.

## Git subprocess rules

Every Git invocation runs with a per-call deadline and output cap, NUL-separated
parsing, `-c core.hooksPath=<empty dir> -c core.fsmonitor=false`, and the
environment `GIT_TERMINAL_PROMPT=0 GIT_OPTIONAL_LOCKS=0`. The server never
fetches and never relies on interactive prompts. A timeout or cancellation after
a mutation was dispatched is `outcome_unknown`, never an automatic replay.

## Budgets

Inventory lists registrations without status or size walks. Inspection bounds
each expensive probe and the total pass; reports bound concurrency and total
time; mutations (create, remove, prune) have a 30-second deadline including
lock waits and verification. List pages are keyset-paginated with at most 20
rows: the cursor is scope plus last key, with no stored snapshots; a restart
invalidates cursors. Response budgets follow `docs/MCP_RESPONSE_STANDARD.md`.

## Tools

| Tool | Effect | Contract |
|---|---|---|
| `get_status` | read | product identity and qualification status |
| `create_worktree` | external-write | repo, name, base, branch/detached; returns exact id/path/ref; conflicts never overwrite |
| `list_worktrees` | read | repo/scope, cursor, limit ≤ 20; inventory with advice and coverage |
| `inspect_worktree` | read | by id or path; bounded evidence, partial when probes degrade |
| `remove_worktree` | external-write | preview or apply(fingerprint); `disposable_paths`, `allow_unmerged`; ≤ 2 KiB receipt |
| `prune_worktrees` | external-write | `{repo, dry_run}`; repository-scope preview or receipt |

## Presentation and delivery

The authoritative tool registry is Rust code in `src/tools/mod.rs`;
`schemas/tools.json` is an exported snapshot, not a load path. Responses are
rendered from typed views through the embedded MiniJinja renderer in
`src/response.rs` per `docs/MCP_RESPONSE_STANDARD.md`. The binary includes an
installer (`self-install`, `releases` subcommands). The release workflow is
disabled and packaging currently refuses non-`none` state, so until the
stateful delivery profile lands, installation is from local builds only.

## Module ownership

| Files | Scope |
|---|---|
| `src/worktree.rs` | shared types, pure policy (`classify`, `assess_removal`, `fingerprint`, validation) |
| `src/git.rs` | Git subprocess access: `common_dir`, `inventory`, `observe`, `create`, `remove`, `prune`, `live_processes_under`, `measure_size` |
| `src/store.rs` | layout/config resolution, per-repo lock, revision-checked record writes, known-repo registry, discovery |
| `src/tools/*`, `src/response.rs`, `src/main.rs` | MCP tool surface, rendering, wiring |

Currently the contract modules exist with substitute bodies that fail closed
(`not_implemented` errors; removal always refused; classification unknown), and
the tool surface beyond `get_status` is not yet registered.
