# Pointing harnesses at the common worktree root

`agent-worktree` keeps every worktree it creates under one root:

```text
<root>/<label>--<id12>/<name>/
```

with `<root>` = `~/.agent-worktree/worktrees` by default, overridable by
`AGENT_WORKTREE_ROOT` or `[storage] root` in `~/.agent-worktree/config.toml`.
The point of one shared root is that both the orchestrating agent (via this
MCP server) and the harness's own worktree feature land in the same place, so
inventory, inspection and preview→apply removal cover everything.

## Codex

Documented, not tested against a specific Codex build.

1. Open Settings → Worktrees.
2. Set **Worktree root** to the same directory this product uses:
   `~/.agent-worktree/worktrees`.
3. Keep the harness's own branch naming; this product classifies harness-made
   worktrees as `foreign` (registered with Git, no record) and never treats
   them as managed, so `remove_worktree` still previews them with full checks.

## Claude Code

Documented, not tested — the hook below is the documented shape; verify it in
a disposable repository before relying on it.

Claude Code creates worktrees through a `WorktreeCreate` hook. Point it at the
same root with a small shell hook (example for macOS/Linux, `~/.claude/settings.json`):

```json
{
  "hooks": {
    "WorktreeCreate": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "aw-create-hook"
          }
        ]
      }
    ]
  }
}
```

with `aw-create-hook` on `PATH`:

```sh
#!/bin/sh
# Reads the worktree request as JSON on stdin; creates the worktree under the
# agent-worktree root; prints the absolute path on stdout.
set -eu
root="${AGENT_WORKTREE_ROOT:-$HOME/.agent-worktree/worktrees}"
repo=$(jq -r '.repo // .cwd // empty')
name=$(jq -r '.name // empty')
[ -n "$repo" ] && [ -n "$name" ] || exit 1
dest="$root/harness/$name"
git -C "$repo" worktree add "$dest" && printf '%s\n' "$dest"
```

Notes:

- The hook uses `git worktree add` directly, so those trees are `foreign` to
  this product: listed and inspected like any registration, and removable only
  through `remove_worktree` under the same vetoes — never auto-deleted.
- Adopting an existing harness directory is not supported; `create_worktree`
  refuses an existing destination by design. Either keep harness-made trees
  foreign, or let the agent use `create_worktree` instead of the harness
  feature and hand the returned path back to the harness.
