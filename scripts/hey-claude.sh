#!/bin/sh
# hey-claude.sh — edit the IN-PROGRESS PROMPT by voice.
#
# Scope (deliberately narrow): the agent acts ONLY on the text the user is
# currently dictating into the focused input field, before it is sent. It reads
# that text, applies a spoken instruction (edit / append / submit / clear), and
# writes the result back. No files, no other panes, no terminal actions.
#
# Field I/O is via the clipboard + ydotool, so it works in ANY focused field
# (Claude Code box, shell, GUI), at the cost of clobbering the clipboard (the user
# runs voxtype with restore_clipboard=false, so this is acceptable).
#
# Reply contract (JSON ops, FAIL-CLOSED — see rewrite-prompt.txt):
#   {"op":"replace","text":"..."}  replace the whole field
#   {"op":"append","text":"..."}   add to the end
#   {"op":"submit"}                 press Enter
#   {"op":"clear"}                  erase the field
#   {"op":"none"}                   do nothing
# Anything unparseable => do nothing + notify. Never apply a malformed reply.
#
# Env: CP_SESSION (voxtype), CP_BIN, CP_TIMEOUT (30000), YDOTOOL_SOCKET,
#      HC_TYPE_DELAY (ms between typed chars, default 0).
# Exit: 0 acted/none; 1 nothing actionable; 3 claude-pipe down.

set -eu

HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
CP_SESSION="${CP_SESSION:-voxtype}"
CP_TIMEOUT="${CP_TIMEOUT:-30000}"
export YDOTOOL_SOCKET="${YDOTOOL_SOCKET:-/run/user/$(id -u)/.ydotool_socket}"

# Debug trace (opt-in: set HC_DEBUG=1). Logs each run's pane/read/response/op to
# $XDG_RUNTIME_DIR/hey-claude.log. Off by default — it would record field contents.
HC_LOG="${XDG_RUNTIME_DIR:-/tmp}/hey-claude.log"
if [ -n "${HC_DEBUG:-}" ]; then
    dbg() { printf '%s %s\n' "$(date '+%H:%M:%S' 2>/dev/null || echo '?')" "$*" >> "$HC_LOG" 2>/dev/null || true; }
else
    dbg() { :; }
fi
dbg "=== run: ZELLIJ=${ZELLIJ:-unset} args=[$*] ==="

if [ -n "${CP_BIN:-}" ]; then :
elif [ -x "$HERE/../target/release/claude-pipe" ]; then CP_BIN="$HERE/../target/release/claude-pipe"
elif command -v claude-pipe >/dev/null 2>&1; then CP_BIN="claude-pipe"
else echo "hey-claude: claude-pipe not found" >&2; exit 3; fi

notify() { command -v notify-send >/dev/null 2>&1 && notify-send -a "hey-claude" "$1" "${2:-}" || true; }

# key chords (Linux input-event codes for `ydotool key`)
K_KILLLINE='29:1 22:1 22:0 29:0' # Ctrl+U — kill whole input line (readline + TUI boxes)
K_END='107:1 107:0'              # End
K_ENTER='28:1 28:0'              # Enter
K_SELALL='29:1 30:1 30:0 29:0'   # Ctrl+A — select-all (GUI fields only)
K_COPY='29:1 46:1 46:0 29:0'     # Ctrl+C — copy (GUI fields only)

# shellcheck disable=SC2086
chord() { ydotool key $1; }

# --- instruction --------------------------------------------------------------
if [ $# -ge 1 ]; then instruction="$*"; else instruction="$(cat)"; fi
instruction="$(printf '%s' "$instruction" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
[ -n "$instruction" ] || { echo "hey-claude: empty instruction" >&2; exit 1; }

# --- read the current field (HYBRID: zellij dump-screen, else clipboard) -------
# In a zellij pane (the Claude box or a shell) we read non-destructively with
# dump-screen — crucially this avoids Ctrl+A/Ctrl+C, which in a TUI mean
# go-to-start / interrupt, NOT select-all / copy (that was corrupting the field
# and could interrupt Claude). Only for non-zellij GUI fields do we fall back to
# the clipboard route.
# Resolve the focused zellij pane. We do NOT gate on $ZELLIJ: when invoked from
# voxtype's gate (a daemon/systemd context) $ZELLIJ is unset even though zellij is
# running. Instead, ask zellij directly; if it answers with a focused terminal
# pane, use the zellij path. `--session` pins the session when not attached.
pane_id=""
if command -v zellij >/dev/null 2>&1; then
    # Pick the LIVE session: $ZELLIJ_SESSION_NAME if set, else the first session
    # that is not marked EXITED (list-sessions tags dead ones "EXITED"). Strip
    # ANSI color codes from the listing before matching.
    zsession="${ZELLIJ_SESSION_NAME:-}"
    if [ -z "$zsession" ]; then
        zsession="$(zellij list-sessions 2>/dev/null \
            | sed 's/\x1b\[[0-9;]*m//g' \
            | grep -v 'EXITED' \
            | awk 'NF{print $1; exit}')"
    fi
    zargs=""
    [ -n "$zsession" ] && zargs="--session $zsession"
    # shellcheck disable=SC2086
    pane_id="$(zellij $zargs action list-panes --all --json 2>/dev/null | python3 -c '
import sys, json
try: panes = json.load(sys.stdin)
except Exception: sys.exit(0)
for p in panes:
    if p.get("is_focused") and not p.get("is_plugin"):
        print("terminal_%s" % p["id"]); break
' 2>/dev/null)"
fi

if [ -n "$pane_id" ]; then
    # Extract the ❯ input line from the rendered viewport. For the Claude box it
    # is the "❯ ..." line that sits between two horizontal-rule lines near the
    # bottom; for a shell it is the last "❯ "/"$ " prompt line. We take the LAST
    # line that begins (after the prompt marker) the input, strip the marker.
    current="$(zellij $zargs action dump-screen --pane-id "$pane_id" 2>/dev/null | python3 -c '
import sys
lines = [l.rstrip("\n") for l in sys.stdin]
# find the last line whose first non-space char is a prompt marker
cand = ""
for l in lines:
    s = l.lstrip()
    for m in ("❯ ", "❯", "$ ", "# ", "% "):
        if s.startswith(m):
            cand = s[len(m):]
            break
sys.stdout.write(cand.strip())
' 2>/dev/null)"
else
    # GUI field: select-all + copy, read the selection.
    chord "$K_SELALL"; chord "$K_COPY"
    current="$(wl-paste --no-newline 2>/dev/null || true)"
fi

dbg "pane_id=[$pane_id] read current=[$current]"

# --- ask the agent ------------------------------------------------------------
resp="$(printf 'CURRENT_PROMPT:\n%s\n\nINSTRUCTION:\n%s' "$current" "$instruction" \
    | "$CP_BIN" send --session "$CP_SESSION" --timeout-ms "$CP_TIMEOUT" --json 2>/dev/null || true)"
dbg "agent resp=[$resp]"
if [ -z "$resp" ]; then
    notify "Hey Claude: agent unavailable" "Is the pipe up? (systemctl --user start claude-pipe-voxtype)"
    echo "hey-claude: no response from claude-pipe ($CP_SESSION)" >&2; exit 3
fi

# --- parse envelope -> normalized plan, FAIL CLOSED ---------------------------
plan="$(printf '%s' "$resp" | python3 -c '
import sys, json
def none(r): print(json.dumps({"op":"none","_err":r})); sys.exit(0)
try: env = json.load(sys.stdin)
except Exception as e: none("env:%s" % e)
if not env.get("ok"): none("not-ok:%s" % env.get("error",""))
text = (env.get("text") or "").strip()
if text.startswith("```"):
    text = text.strip("`")
    if text[:4].lower() == "json": text = text[4:]
    text = text.strip()
try: op = json.loads(text)
except Exception as e: none("json:%s" % e)
if not isinstance(op, dict): none("obj")
o = op.get("op")
if o in ("replace","append"):
    if not isinstance(op.get("text"), str): none(o+"-no-text")
    print(json.dumps({"op":o,"text":op["text"]}))
elif o in ("submit","clear","none"):
    print(json.dumps({"op":o}))
else: none("bad-op")
')"
op="$(printf '%s' "$plan" | python3 -c 'import sys,json;print(json.load(sys.stdin)["op"])')"
get_text() { printf '%s' "$plan" | python3 -c 'import sys,json;sys.stdout.write(json.load(sys.stdin).get("text",""))'; }
dbg "plan=[$plan] op=[$op]"

# --- apply --------------------------------------------------------------------
# In a zellij pane, address the pane DIRECTLY with `zellij action write` — no focus
# dependency, no race, reliable (ydotool key injection was unreliable here: keys
# went to the wrong target / didn't land). Only GUI fields fall back to ydotool.
# zellij byte codes: Ctrl-U=21, Enter=13, End=control-seq (we use write-chars+nav).
if [ -n "$pane_id" ]; then
    zkill() { zellij $zargs action write --pane-id "$pane_id" 21 >/dev/null 2>&1; }   # Ctrl-U
    zenter() { zellij $zargs action write --pane-id "$pane_id" 13 >/dev/null 2>&1; }  # Enter
    ztype() { zellij $zargs action write-chars --pane-id "$pane_id" "$1" >/dev/null 2>&1; }
    case "$op" in
        none)    : ;;
        submit)  zenter ;;
        clear)   zkill ;;
        replace) zkill; ztype "$(get_text)" ;;
        append)  ztype "$(get_text)" ;;   # write-chars inserts at cursor (line end)
    esac
else
    case "$op" in
        none)    : ;;
        submit)  chord "$K_ENTER" ;;
        clear)   chord "$K_KILLLINE" ;;
        replace) chord "$K_KILLLINE"; ydotool type -- "$(get_text)" ;;
        append)  chord "$K_END"; ydotool type -- "$(get_text)" ;;
    esac
fi
exit 0
