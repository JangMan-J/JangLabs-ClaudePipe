#!/bin/sh
# zellij-field.sh — read/replace the in-progress input line of the FOCUSED zellij
# pane, using only `zellij action` (no clipboard, no ydotool, no focus changes).
#
# This is the field-capture/replace primitive for the claude-pipe voxtype Siri
# consumer (Thread 2). The target is "the command line I'm currently typing" at a
# shell prompt. It deliberately does NOT try to scrape input out of full-screen
# alternate-screen TUIs (Claude Code, nvim) — those manage their own input and the
# rendered viewport is not a reliable source for the in-progress line.
#
# Subcommands:
#   zellij-field.sh id        -> prints the focused pane id (e.g. terminal_12) or exits 1
#   zellij-field.sh kind      -> prints "shell" | "tui" | "unknown" for the focused pane
#   zellij-field.sh read      -> prints the in-progress input line (prompt stripped) or exits 1
#   zellij-field.sh replace TEXT
#                             -> clears the focused pane's input line and writes TEXT
#
# Verified on zellij 0.45.0. Facts the design rests on (all checked empirically):
#   * `list-panes --all --json` exposes is_focused, is_plugin, id. JSON's
#     terminal_command is unreliable (None for Claude) — use the table COMMAND col.
#   * `dump-screen --pane-id <id>` prints the viewport to STDOUT, non-destructive,
#     no focus needed.
#   * `write --pane-id <id> 21` sends Ctrl-U (zsh kill-whole-line) to clear input.
#   * `write-chars --pane-id <id> TEXT` types TEXT into the pane.
#
# Exit codes: 0 ok; 1 no focused/eligible pane or empty read; 2 usage error.
#
# CAVEAT: capture relies on the prompt containing one of "❯ $ # %". This matches
# the user's zsh prompt (uses ❯). A custom prompt with none of these markers would
# cause the whole rendered line (prompt included) to be captured. Verified edge
# cases: empty line, wrapped 200+ char line (dump-screen reconstructs it), and a
# command body that itself contains ❯/$/# (we strip the FIRST marker, not last).

set -eu

# --- locate the focused pane id (terminal_N / plugin_N) -----------------------
focused_id() {
    zellij action list-panes --all --json 2>/dev/null | python3 -c '
import sys, json
try:
    panes = json.load(sys.stdin)
except Exception:
    sys.exit(1)
for p in panes:
    if p.get("is_focused"):
        kind = "plugin" if p.get("is_plugin") else "terminal"
        print("%s_%s" % (kind, p["id"]))
        sys.exit(0)
sys.exit(1)
'
}

# --- classify the focused pane by its launch COMMAND (table is authoritative) --
# Shells we treat as text-entry targets; everything else (claude, nvim, vim, ...)
# is a TUI we refuse to scrape. The COMMAND column is field 7 in the --all table.
focused_kind() {
    id="$1"
    cmd="$(zellij action list-panes --all 2>/dev/null \
        | awk -F'  +' -v id="$id" '$4==id {print $7; exit}')"
    case "$cmd" in
        */zsh|*/bash|*/sh|*/fish|*/dash|*/ash) echo shell ;;
        "" ) echo unknown ;;
        * ) echo tui ;;
    esac
}

# --- extract the in-progress input line from a shell pane dump -----------------
# Strategy: take the LAST non-empty rendered line. It is "<prompt>❯ <input>".
# Strip everything up to and including the FIRST prompt marker "❯ " (or "$ "/"# "
# as fallbacks). What remains is the in-progress command (possibly empty).
# We use the FIRST occurrence, not the last: the prompt marker is at the start of
# the line, so a command that itself contains "❯"/"$"/"#" (e.g. echo "a ❯ b")
# must not have its body truncated by matching an embedded marker.
read_line() {
    id="$1"
    zellij action dump-screen --pane-id "$id" 2>/dev/null | python3 -c '
import sys
lines = [l.rstrip("\n") for l in sys.stdin]
# last non-empty (after stripping trailing spaces) line
line = ""
for l in reversed(lines):
    if l.strip():
        line = l
        break
# strip prompt prefix up to and including the FIRST marker found
best = None  # (index, marker_len)
for marker in ("❯ ", "❯", "$ ", "# ", "% "):
    idx = line.find(marker)
    if idx != -1 and (best is None or idx < best[0]):
        best = (idx, len(marker))
if best is not None:
    line = line[best[0] + best[1]:]
print(line, end="")
'
}

# --- replace the input line: Ctrl-U to clear, then type the new text ----------
replace_line() {
    id="$1"; text="$2"
    zellij action write --pane-id "$id" 21 >/dev/null 2>&1   # Ctrl-U
    zellij action write-chars --pane-id "$id" "$text" >/dev/null 2>&1
}

# --- dispatch -----------------------------------------------------------------
[ $# -ge 1 ] || { echo "usage: $0 {id|kind|read|replace TEXT}" >&2; exit 2; }
cmd="$1"; shift

case "$cmd" in
    id)
        id="$(focused_id)" || { echo "no focused pane" >&2; exit 1; }
        printf '%s\n' "$id"
        ;;
    kind)
        id="$(focused_id)" || { echo "no focused pane" >&2; exit 1; }
        focused_kind "$id"
        ;;
    read)
        id="$(focused_id)" || { echo "no focused pane" >&2; exit 1; }
        kind="$(focused_kind "$id")"
        [ "$kind" = shell ] || { echo "focused pane is not a shell ($kind)" >&2; exit 1; }
        out="$(read_line "$id")"
        [ -n "$out" ] || exit 1
        printf '%s' "$out"
        ;;
    replace)
        [ $# -ge 1 ] || { echo "usage: $0 replace TEXT" >&2; exit 2; }
        id="$(focused_id)" || { echo "no focused pane" >&2; exit 1; }
        kind="$(focused_kind "$id")"
        [ "$kind" = shell ] || { echo "focused pane is not a shell ($kind)" >&2; exit 1; }
        replace_line "$id" "$1"
        ;;
    *)
        echo "usage: $0 {id|kind|read|replace TEXT}" >&2; exit 2
        ;;
esac
