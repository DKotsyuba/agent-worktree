# Security boundary

`agent-worktree` performs local Git mutations only, on repositories the caller
names. It is a stdio MCP server: no network client, no fetch or push, no
daemon, no credential handling. Git subprocesses run with a deadline and output
cap, with `GIT_TERMINAL_PROMPT=0` and with `core.hooksPath` pointed at an empty
directory and `core.fsmonitor=false`, so repository hooks and monitors never
execute on its account.

## What may be deleted

- `remove_worktree` deletes exactly one worktree directory (working files,
  plus ignored files explicitly approved through `disposable_paths`) and its
  local record — only after a preview whose fingerprint still matches at apply
  time. Deletion is permanent; there is no trash or quarantine.
- `prune_worktrees` removes Git worktree registrations of missing directories
  only — never branches, never existing directories.

Never deleted: branches (commits are always retained), the main worktree or a
bare repository root, anything outside the named worktree path, records of
other worktrees.

## Refusal rules

Removal and pruning refuse on: dirty tracked state, untracked files, merge
conflicts, dirty or unsupported submodules; ignored files outside the approved
`disposable_paths`; a Git-locked worktree; a live process inside the tree or
insufficient process coverage; the main worktree or bare root; an unmerged HEAD
without the explicit `allow_unmerged` consent; any fingerprint divergence
between preview and apply. Git is never passed `--force`; if Git itself
refuses, the outcome is reported, not forced through.

## Attribution, not authentication

Caller-supplied harness and session names (`creator`, `session`) are recorded
attribution, not authentication or authorization. The server acts with the
permissions of the user running the MCP host; whoever can call it can ask for
the mutations above within these rules.

Secrets never enter responses or diagnostics; paths, labels and free-text
fields are bounded and validated before they reach state or Git.
