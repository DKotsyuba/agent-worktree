# Idle-worktree notifications

`agent-worktree hook context` is a `UserPromptSubmit` hook command for host
harnesses (Claude Code, Codex). When a linked worktree crosses 24 h without
activity, it injects ONE message into the orchestrator's context, wrapped in
`<agent-worktree>…</agent-worktree>`. It never repeats itself for the same
idle episode, and the orchestrator decides what to do — the hook itself only
reads: no removal, no prune, no state mutation beyond its own bookkeeping
file. There are no deadlines or TTLs on worktrees anywhere in the product.

## Behaviour

- Scan scope is exactly `list_worktrees` without a repo filter: the
  known-repo registry plus configured `[discovery] roots`.
- Only existing, non-main linked worktrees are considered; bare roots and
  prunable registrations are skipped, and the main checkout never notifies.
- Signals are the cheap ones only: HEAD/index mtimes plus the timestamp of
  the last HEAD reflog entry — the same cheap pass list pages use. No status
  walk, no `lsof`, no size measurement. Missing signals mean unknown, and
  unknown is never proof of abandonment, so such worktrees are skipped.
- Idle means the newest signal is at least `Policy::idle_after_secs`
  (24 h) old. Mergedness is one bounded `merge-base` against the
  integration ref and prints as `merged`, `unmerged` or `unknown`.
- Output (only when there is something new), ≤ 12 lines and ≤ 1.5 KiB:

  ```text
  <agent-worktree>
  2 worktree(s) have had no activity for over 24 h:
  - myrepo/task-9 — idle 3 d, unmerged, /wt/root/myrepo--abc123/task-9
  - otherrepo/spike — idle 9 d, merged, /wt/root/otherrepo--def456/spike
  Review: list_worktrees; remove: remove_worktree preview → apply.
  </agent-worktree>
  ```

  Up to 10 rows are listed; anything beyond collapses into a `+N more`
  line, and the 12-line / 1.5 KiB caps win over the row cap.

- The block is emitted as one `UserPromptSubmit` JSON envelope on stdout,
  the same shape `agent-run hook context` emits for both hosts:

  ```json
  {"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"<agent-worktree>\n…"}}
  ```

  `--host claude|codex` (default `claude`) labels the harness in
  diagnostics; both hosts currently consume the identical envelope.

## Host safety

- The command always exits 0 and prints nothing on any internal error; one
  bounded line is appended to `<home>/logs/hook.log` instead (the log is
  halved once it passes 64 KiB).
- Everything runs under a 3 s total deadline: a scan that does not finish
  in time prints nothing and exits 0. A partial scan never guesses.
- Delegate guard: agent-run marks its launched children with
  `AGENT_RUN_WORKER_*` environment variables; when any of them is present
  the hook prints nothing — a delegated worker's orchestrator already owns
  worktree hygiene.

## Rate limit

Scans run at most once per 10 minutes. The timestamp of the last completed
scan is kept in the state file; a prompt arriving inside the window prints
nothing and exits immediately (one config read plus one small state read —
a few milliseconds, no Git subprocess). A pass that runs out of its scan
budget (for example a cold, very large discovery root) is incomplete: it
prints nothing and writes no state at all, so the next prompt rescans
instead of being rate-limited by a partial answer.

## State file and episode rule

State lives in `<home>/state/v1/notify.json`, guarded by an advisory
exclusive lock at `<home>/state/v1/notify.lock` and written atomically:

```json
{"schema_version":1,"last_scan_at":1790000000,
 "episodes":[{"path":"/wt/root/myrepo--abc123/task-9","last_activity":1789800000}]}
```

An **episode** is the pair (canonical worktree path, last-activity
timestamp). Only episodes not yet recorded in the state file are announced;
the scan then rewrites the file with the currently idle set, which also
drops entries for worktrees that no longer exist (or are no longer idle).
If a worktree becomes active and later idle again, its timestamp moved, so
that is a new episode and it notifies again — once.

## Hook entries

See [harness-setup.md](harness-setup.md) for the exact Claude Code settings
JSON and the crew `[[hooks]]` entries for Claude Code and Codex.
