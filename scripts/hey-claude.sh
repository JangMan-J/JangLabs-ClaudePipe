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

if [ -n "${CP_BIN:-}" ]; then :
elif [ -x "$HERE/../target/release/claude-pipe" ]; then CP_BIN="$HERE/../target/release/claude-pipe"
elif command -v claude-pipe >/dev/null 2>&1; then CP_BIN="claude-pipe"
else echo "hey-claude: claude-pipe not found" >&2; exit 3; fi

notify() { command -v notify-send >/dev/null 2>&1 && notify-send -a "hey-claude" "$1" "${2:-}" || true; }

# key chords (Linux input-event codes for `ydotool key`)
K_SELALL='29:1 30:1 30:0 29:0'   # Ctrl+A
K_COPY='29:1 46:1 46:0 29:0'     # Ctrl+C
K_DELETE='111:1 111:0'           # Delete
K_END='107:1 107:0'              # End
K_ENTER='28:1 28:0'              # Enter

# shellcheck disable=SC2086
chord() { ydotool key $1; }

# --- instruction --------------------------------------------------------------
if [ $# -ge 1 ]; then instruction="$*"; else instruction="$(cat)"; fi
instruction="$(printf '%s' "$instruction" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
[ -n "$instruction" ] || { echo "hey-claude: empty instruction" >&2; exit 1; }

# --- read the current field via clipboard -------------------------------------
# select-all + copy, then read the selection. Small settle delays so the chord and
# the clipboard write land before we read.
chord "$K_SELALL"; chord "$K_COPY"
current="$(wl-paste --no-newline 2>/dev/null || true)"

# --- ask the agent ------------------------------------------------------------
resp="$(printf 'CURRENT_PROMPT:\n%s\n\nINSTRUCTION:\n%s' "$current" "$instruction" \
    | "$CP_BIN" send --session "$CP_SESSION" --timeout-ms "$CP_TIMEOUT" --json 2>/dev/null || true)"
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

# --- apply --------------------------------------------------------------------
case "$op" in
    none)
        exit 0 ;;
    submit)
        chord "$K_ENTER" ;;
    clear)
        chord "$K_SELALL"; chord "$K_DELETE" ;;
    replace)
        chord "$K_SELALL"; chord "$K_DELETE"
        ydotool type -- "$(get_text)" ;;
    append)
        chord "$K_END"
        ydotool type -- "$(get_text)" ;;
esac
exit 0
