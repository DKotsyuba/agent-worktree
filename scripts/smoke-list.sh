#!/bin/sh
# Read-only smoke: drives the built binary over stdio and prints the raw
# tool reply (one JSON line). Usage:
#   smoke-list.sh BINARY AGENT_WORKTREE_HOME [TOOL] [arguments JSON object]
# The producer holds the pipe open for HOLD seconds so Git discovery finishes;
# the reader exits as soon as the reply arrives.
set -eu
BIN=$1
AWHOME=$2
TOOL=${3:-list_worktrees}
# NB: a literal brace default must not live inside ${...}; the first "}" would
# close the expansion and append a stray "}" to a supplied argument.
ARGS=${4:-}
[ -n "$ARGS" ] || ARGS='{}'
HOLD=${SMOKE_HOLD_SECS:-30}
mkdir -p "$AWHOME"
{
  printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}\n'
  printf '{"jsonrpc":"2.0","method":"notifications/initialized"}\n'
  printf '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"%s","arguments":%s}}\n' "$TOOL" "$ARGS"
  sleep "$HOLD"
} | AGENT_WORKTREE_HOME="$AWHOME" "$BIN" mcp 2>/dev/null | awk 'match($0, /"id":2/) { print; exit }'
