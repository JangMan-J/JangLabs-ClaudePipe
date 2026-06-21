#!/bin/sh
# hey-claude-up.sh — bring up (keep warm) the persistent claude-pipe agent used by
# the "Hey Claude" voxtype consumer, with the rewrite system prompt baked in.
#
# Idempotent: if the session is already up, this is a fast no-op. Intended to be
# called from voxtype's "Recording started" hook so the agent is hot by the time
# you finish speaking.
#
# Env: CP_SESSION (default voxtype), CP_BIN, CP_MODEL (default: claude's default;
#      set CP_MODEL=claude-haiku-4-5-20251001 for the fastest warm turns).

set -eu

HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
CP_SESSION="${CP_SESSION:-voxtype}"
CP_MODEL="${CP_MODEL:-}"

if [ -n "${CP_BIN:-}" ]; then :
elif [ -x "$HERE/../target/release/claude-pipe" ]; then CP_BIN="$HERE/../target/release/claude-pipe"
elif command -v claude-pipe >/dev/null 2>&1; then CP_BIN="claude-pipe"
else echo "hey-claude-up: claude-pipe not found" >&2; exit 3; fi

# already up? Check the printed liveness, not just the exit code: older
# claude-pipe builds exit 0 from `status` even when dead. We treat the session as
# up only if status reports `live: true`.
if "$CP_BIN" status --session "$CP_SESSION" 2>/dev/null | grep -q '^live:[[:space:]]*true'; then
    exit 0
fi

# The rewrite contract. Kept terse and absolute — the model must emit ONE JSON
# object and nothing else. Examples teach the four ops. "none" is the safe valve.
SYS='You convert a spoken INSTRUCTION about a shell command line into ONE action.
You are given INSTRUCTION (what the user said) and CURRENT_INPUT_LINE (what is
currently typed at their shell prompt; may be empty).

Reply with EXACTLY ONE JSON object and NOTHING else. No prose. No markdown fence.
No explanation. The JSON is consumed by a program and typed into a real shell.

Ops:
  {"op":"replace","text":"<the new command line>"}            replace the line
  {"op":"replace","text":"<the new command line>","submit":true}  replace then run it
  {"op":"keys","keys":["ctrl+a"]}    a pure cursor/edit action, no text change
  {"op":"none"}                       when the instruction is a question, is unclear,
                                      or should not change the shell at all

Rules:
- "text" is the FULL replacement line, not a diff. Preserve the parts the user did
  not ask to change. Output the command exactly as it should appear — no $ or ❯.
- Only set "submit":true when the user clearly asks to run/execute it.
- Allowed keys: enter, ctrl+a, ctrl+e, ctrl+u, ctrl+k, ctrl+w, ctrl+c, home, end,
  tab, escape, backspace.
- If you are unsure, or the instruction is not an edit to the command line, use
  {"op":"none"}. Never guess a destructive command.'

# NOTE: do NOT `exec` here. `up --detach` already daemonizes via setsid(2) and
# returns once the socket is ready. If we exec, this script process BECOMES the
# detached launcher and, on its exit, the process group receives SIGHUP which
# kills the freshly-setsid'd grandchild daemon before it serves a turn (verified:
# exec => daemon dies immediately; plain call => daemon survives). Run it as a
# normal child and let this script return on its own.
# shellcheck disable=SC2086
if [ -n "$CP_MODEL" ]; then
    "$CP_BIN" up --session "$CP_SESSION" --detach --model "$CP_MODEL" --system "$SYS"
else
    "$CP_BIN" up --session "$CP_SESSION" --detach --system "$SYS"
fi
