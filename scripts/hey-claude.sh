#!/bin/sh
# hey-claude.sh — the "Hey Claude" voxtype Siri consumer.
#
# Pipeline: wake-phrase already stripped by the caller (llm-gate.sh). This script
# receives the spoken INSTRUCTION on argv/stdin, captures the focused zellij shell
# pane's in-progress input line, asks the persistent claude-pipe agent to act on
# it, and applies the result back into the SAME pane — all over `zellij action`
# (no clipboard, no ydotool, no focus change).
#
# Reply contract (JSON ops, FAIL-CLOSED). The agent must reply with exactly ONE
# JSON object, no prose, no markdown fence:
#   {"op":"replace","text":"<new line>"}                 fill the input line
#   {"op":"replace","text":"<new line>","submit":true}   fill + press Enter
#   {"op":"keys","keys":["ctrl+a","ctrl+k"]}             pure key action, no text
#   {"op":"none"}                                         refuse / touch nothing
# Anything that does not parse into one of these => DO NOTHING + notify. We never
# type an unparsed reply into the shell.
#
# Usage:
#   hey-claude.sh "make this a one-liner"          # instruction on argv
#   echo "fix the typo" | hey-claude.sh            # instruction on stdin
# Env:
#   CP_SESSION   claude-pipe session name (default: voxtype)
#   CP_BIN       path to claude-pipe (default: looks in repo target/release then PATH)
#   CP_TIMEOUT   per-request timeout ms (default: 30000)
#   HC_FIELD     path to zellij-field.sh (default: alongside this script)
#
# Exit codes: 0 applied (or op:none honored); 1 nothing to act on / fail-closed;
#             2 usage; 3 claude-pipe unavailable.

set -eu

HERE="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
FIELD="${HC_FIELD:-$HERE/zellij-field.sh}"
CP_SESSION="${CP_SESSION:-voxtype}"
CP_TIMEOUT="${CP_TIMEOUT:-30000}"

# locate claude-pipe: repo release build, then PATH
if [ -n "${CP_BIN:-}" ]; then
    :
elif [ -x "$HERE/../target/release/claude-pipe" ]; then
    CP_BIN="$HERE/../target/release/claude-pipe"
elif command -v claude-pipe >/dev/null 2>&1; then
    CP_BIN="claude-pipe"
else
    echo "hey-claude: claude-pipe not found" >&2; exit 3
fi

# notify helper (KDE/desktop); silent if notify-send is missing
notify() { command -v notify-send >/dev/null 2>&1 && notify-send -a "hey-claude" "$1" "${2:-}" || true; }

# --- instruction from argv or stdin -------------------------------------------
if [ $# -ge 1 ]; then
    instruction="$*"
else
    instruction="$(cat)"
fi
instruction="$(printf '%s' "$instruction" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
[ -n "$instruction" ] || { echo "hey-claude: empty instruction" >&2; exit 2; }

# --- branch on the focused pane kind ------------------------------------------
kind="$("$FIELD" kind 2>/dev/null || echo unknown)"
pane_id="$("$FIELD" id 2>/dev/null || true)"

# Claude TUI: do NOT route through the rewrite agent (the pane IS an agent). Type
# the spoken instruction straight into its ❯ input box, and submit if the user
# said so. A trailing submit cue ("and send/submit", "go", "run it", "send it")
# is stripped and turned into Enter.
if [ "$kind" = claude ]; then
    # normalize for whole-instruction control-command matching: lowercase, strip
    # surrounding punctuation/space.
    norm="$(printf '%s' "$instruction" | tr 'A-Z' 'a-z' \
        | sed -E 's/^[[:space:][:punct:]]+//; s/[[:space:][:punct:]]+$//')"

    # CONTROL: clear/scratch the box. The WHOLE instruction is the command
    # (e.g. "clear that", "scratch that", "erase this line", "delete my input").
    case "$norm" in
        clear|scratch|erase|delete \
        |clear\ *that|scratch\ *that|erase\ *that|delete\ *that \
        |clear\ *this*|scratch\ *this*|erase\ *this*|delete\ *this* \
        |clear\ *it|scratch\ *it|erase\ *it|delete\ *it \
        |clear\ *input|delete\ *input|erase\ *input|clear\ *line|erase\ *line|delete\ *line \
        |start\ over|never\ mind|nevermind)
            zellij action write --pane-id "$pane_id" 21 >/dev/null 2>&1   # Ctrl-U
            exit 0
            ;;
    esac

    # CONTROL: submit-only. The WHOLE instruction is the submit verb.
    case "$norm" in
        send|send\ it|submit|submit\ it|go|run\ it|enter)
            zellij action write --pane-id "$pane_id" 13 >/dev/null 2>&1   # Enter
            exit 0
            ;;
    esac

    # Otherwise: type the instruction into the box. A TRAILING submit cue on a
    # longer instruction ("... and send", "... send it") strips + submits.
    submit=""
    rest="$(printf '%s' "$instruction" | sed -E 's/[[:space:],.]*(and[[:space:]]+)?(send( it)?|submit( it)?|go|run it)[[:space:].!]*$//I')"
    [ "$rest" != "$instruction" ] && submit=1
    rest="$(printf '%s' "$rest" | sed -e 's/[[:space:]]*$//')"
    [ -n "$rest" ] || { echo "hey-claude: empty instruction for claude box" >&2; exit 1; }
    zellij action write-chars --pane-id "$pane_id" "$rest" >/dev/null 2>&1
    [ -n "$submit" ] && zellij action write --pane-id "$pane_id" 13 >/dev/null 2>&1
    exit 0
fi

if [ "$kind" != shell ]; then
    notify "Hey Claude: not a shell pane" "Focused pane is '$kind' — no input line to rewrite."
    echo "hey-claude: focused pane is '$kind', not a shell" >&2
    exit 1
fi
# read may legitimately be empty (blank prompt); that's fine — instruction may
# create content from nothing (e.g. "Hey Claude, write a curl to example.com").
field="$("$FIELD" read 2>/dev/null || true)"

# --- build the agent message --------------------------------------------------
# The system prompt is set once at `up` time (see hey-claude-up below). Here we
# send a single structured user message: the instruction + the current field.
msg="$(printf 'INSTRUCTION:\n%s\n\nCURRENT_INPUT_LINE:\n%s' "$instruction" "$field")"

# --- send to the persistent agent (JSON envelope) -----------------------------
resp="$(printf '%s' "$msg" | "$CP_BIN" send --session "$CP_SESSION" --timeout-ms "$CP_TIMEOUT" --json 2>/dev/null || true)"
if [ -z "$resp" ]; then
    notify "Hey Claude: agent unavailable" "Is the pipe up? (claude-pipe up --session $CP_SESSION --detach)"
    echo "hey-claude: no response from claude-pipe (session $CP_SESSION)" >&2
    exit 3
fi

# --- parse envelope -> op, FAIL CLOSED ----------------------------------------
# resp is {"ok":true,"text":"<the agent's reply>",...}. The agent's reply text is
# itself the op JSON. We parse both layers in python; ANY failure => op:none.
plan="$(printf '%s' "$resp" | python3 -c '
import sys, json
def fail(reason):
    print(json.dumps({"op":"none","_err":reason})); sys.exit(0)
try:
    env = json.load(sys.stdin)
except Exception as e:
    fail("envelope:%s" % e)
if not env.get("ok"):
    fail("not-ok:%s" % env.get("error",""))
text = (env.get("text") or "").strip()
# tolerate a single ```json ... ``` fence if the model adds one
if text.startswith("```"):
    text = text.strip("`")
    if text[:4].lower() == "json": text = text[4:]
    text = text.strip()
try:
    op = json.loads(text)
except Exception as e:
    fail("reply-json:%s" % e)
if not isinstance(op, dict) or op.get("op") not in ("replace","keys","none"):
    fail("bad-op")
# normalize/validate per op
o = op.get("op")
if o == "replace":
    if not isinstance(op.get("text"), str): fail("replace-no-text")
    print(json.dumps({"op":"replace","text":op["text"],"submit":bool(op.get("submit"))}))
elif o == "keys":
    ks = op.get("keys")
    if not isinstance(ks, list) or not all(isinstance(k,str) for k in ks) or not ks:
        fail("keys-bad")
    print(json.dumps({"op":"keys","keys":ks}))
else:
    print(json.dumps({"op":"none"}))
')"

op="$(printf '%s' "$plan" | python3 -c 'import sys,json;print(json.load(sys.stdin)["op"])')"

# --- chord name -> zellij write byte(s) ---------------------------------------
# Only a small, safe allowlist. Unknown chord => skip it (already fail-closed at
# parse for structure; here we just ignore names we do not know).
chord_bytes() {
    case "$1" in
        enter|return|ctrl+m) echo 13 ;;
        ctrl+a|home)         echo 1 ;;
        ctrl+e|end)          echo 5 ;;
        ctrl+u)              echo 21 ;;
        ctrl+k)              echo 11 ;;
        ctrl+w)              echo 23 ;;
        ctrl+c)              echo 3 ;;
        tab)                 echo 9 ;;
        escape|esc)          echo 27 ;;
        backspace)           echo 127 ;;
        *)                   echo "" ;;
    esac
}

# --- apply --------------------------------------------------------------------
case "$op" in
    none)
        # honored refusal — type nothing
        exit 0
        ;;
    replace)
        text="$(printf '%s' "$plan" | python3 -c 'import sys,json;sys.stdout.write(json.load(sys.stdin)["text"])')"
        submit="$(printf '%s' "$plan" | python3 -c 'import sys,json;print("1" if json.load(sys.stdin)["submit"] else "")')"
        "$FIELD" replace "$text"      # clears line (Ctrl-U) then types text
        if [ -n "$submit" ]; then
            zellij action write --pane-id "$pane_id" 13 >/dev/null 2>&1
        fi
        exit 0
        ;;
    keys)
        # iterate keys; map each to a byte and write it
        printf '%s' "$plan" | python3 -c 'import sys,json;[print(k) for k in json.load(sys.stdin)["keys"]]' \
        | while IFS= read -r k; do
            b="$(chord_bytes "$k")"
            [ -n "$b" ] && zellij action write --pane-id "$pane_id" "$b" >/dev/null 2>&1
        done
        exit 0
        ;;
esac
