#!/bin/sh
# hey-claude-up.sh — bring up (keep warm) the persistent claude-pipe agent used by
# the "Hey Claude" voxtype consumer, with the rewrite system prompt baked in.
#
# Default: idempotent detached launch — if the session is already up this is a
# fast no-op. Intended for a PTT "record start" hook so the agent is hot by the
# time you finish speaking.
#
# --foreground: run claude-pipe up in the FOREGROUND (no --detach) so a service
# manager (systemd) can supervise and restart it. Skips the idempotency check.
#
# Single source of truth for the rewrite contract: scripts/rewrite-prompt.txt.
#
# Env: CP_SESSION (default voxtype), CP_BIN, CP_MODEL (default: claude's default;
#      set CP_MODEL=claude-haiku-4-5-20251001 for the fastest warm turns).

set -eu

HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
CP_SESSION="${CP_SESSION:-voxtype}"
CP_MODEL="${CP_MODEL:-}"
PROMPT_FILE="${HC_PROMPT_FILE:-$HERE/rewrite-prompt.txt}"

foreground=0
[ "${1:-}" = "--foreground" ] && foreground=1

if [ -n "${CP_BIN:-}" ]; then :
elif [ -x "$HERE/../target/release/claude-pipe" ]; then CP_BIN="$HERE/../target/release/claude-pipe"
elif command -v claude-pipe >/dev/null 2>&1; then CP_BIN="claude-pipe"
else echo "hey-claude-up: claude-pipe not found" >&2; exit 3; fi

[ -r "$PROMPT_FILE" ] || { echo "hey-claude-up: prompt file not readable: $PROMPT_FILE" >&2; exit 3; }
SYS="$(cat "$PROMPT_FILE")"

# detached mode: skip if already live. Check the PRINTED liveness, not just exit
# code (older claude-pipe builds exit 0 from `status` even when dead).
if [ "$foreground" -eq 0 ]; then
    if "$CP_BIN" status --session "$CP_SESSION" 2>/dev/null | grep -q '^live:[[:space:]]*true'; then
        exit 0
    fi
fi

set -- up --session "$CP_SESSION" --system "$SYS"
[ -n "$CP_MODEL" ] && set -- "$@" --model "$CP_MODEL"
[ "$foreground" -eq 0 ] && set -- "$@" --detach

# foreground: exec so systemd supervises claude-pipe directly (it then owns the
# daemon; no detached grandchild to lose). detached: run as a normal child and
# return — do NOT exec (exec + --detach => the freshly setsid'd daemon gets
# SIGHUP on this script's exit and dies before serving a turn).
if [ "$foreground" -eq 1 ]; then
    exec "$CP_BIN" "$@"
else
    "$CP_BIN" "$@"
fi
