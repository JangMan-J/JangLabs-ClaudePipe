# claude-pipe

A lean, standalone **request/reply pipe to a persistent Claude Code agent**.

Spawning `claude -p` per call pays a ~2.4 s cold start every time. `claude-pipe`
spawns **one long-lived `claude` process** in streaming-JSON mode and relays
messages to it over a Unix-domain socket. You pay the cold start **once**, at
`up`; every `send` afterwards costs only Claude's inference latency.

Measured on this machine (Haiku, warm, **lean default**): **~0.9 s/turn**, vs
~2.5 s for `claude -p` cold-start, with full **context retained** across turns
(it's one continuous conversation).

No terminal multiplexer in the hot path: we own the process, so we talk to its
stdin/stdout directly. This is deliberately a thin, reusable **platform** — the
only thing a consumer needs is `claude-pipe send "<text>"`.

## Build

```sh
cargo build --release
# binary at target/release/claude-pipe
```

## Use

```sh
# Start a persistent agent (one process + one socket per session name).
claude-pipe up --detach \
  --model claude-haiku-4-5-20251001 \
  --system "You are terse. Reply with only the answer."

# Send a message; blocks until the turn completes; prints the reply.
claude-pipe send "press control u"
echo "transcribe this" | claude-pipe send          # message from stdin
claude-pipe send --json "..."                        # full response envelope

# Inspect / stop.
claude-pipe status   # exits 0 if the session is live, 1 otherwise (usable as a shell predicate)
claude-pipe down
```

Named sessions run independently, so different projects/purposes don't collide:

```sh
claude-pipe up --detach --session dictation
claude-pipe send --session dictation "..."
```

## The wire contract (all a consumer depends on)

Request (client → daemon), one JSON line over the socket:

```json
{"text": "press control u", "timeout_ms": 60000}
```

Response (daemon → client), one JSON line:

```json
{"ok": true, "text": "<claude reply>", "session_id": "abc123", "turn_ms": 1100}
```

On error/timeout: `{"ok": false, "text": "", "error": "timeout after …"}`.

`claude-pipe send` is just a thin client over this; you can speak the protocol
directly from any language by connecting to the socket.

## How it works

```
  client (any consumer)                     claude-pipe daemon
  ┌──────────────────────┐                 ┌──────────────────────────────────┐
  │ claude-pipe send "…" │ ── UDS req ───► │ serializing worker                │
  │   (blocks for reply) │                 │  owns: claude -p --verbose         │
  │                      │ ◄── UDS reply ─ │    --input-format stream-json      │
  └──────────────────────┘                 │    --output-format stream-json     │
                                            │    [--model] [--resume <sid>]      │
                                            │  write {"type":"user",…} → stdin   │
                                            │  read events until {"type":        │
                                            │    "result", …}  (turn sentinel)   │
                                            └──────────────────────────────────┘
```

- **Serialization.** Claude's streaming stdin/stdout is a single ordered
  channel, so the daemon runs **one turn at a time** (a worker behind an mpsc
  queue). Correct for sequential workloads like dictation; replies can't
  interleave.
- **Turn completion** is detected by Claude's own `result` event — no
  transcript-tailing, no prompt-ready heuristics, no screen scraping.
- **Lean by default.** The persistent `claude` is launched with no tools, no
  MCP servers, no settings/hooks, and a replaced (minimal) system prompt. This
  drops per-turn cache tokens from ~8.7k created / ~17.7k read to **0** and stops
  the `SessionStart` hook from firing every turn — pure overhead for a pipe that
  only relays text. Turn detection is unaffected because it keys off Claude's
  `result` event, not any hook. Pass `--full` to restore Claude's normal agent
  loadout for consumers that need Claude to *act* (run tools, use MCP), not just
  transform text.
- **Persistence.** The `session_id` is saved to a state file; restart with
  `--resume <session_id>` to continue the same conversation.
- **Timeout recovery.** If a turn exceeds `timeout_ms`, its `result` is still
  pending on the stream; the daemon **drains** that stale output before serving
  the next request, so the stream never desyncs (replies stay aligned).

## Runtime files

`$XDG_RUNTIME_DIR/claude-pipe/` (fallback `/tmp/claude-pipe-$UID/`):

- `<session>.sock` — the request/reply socket
- `<session>.state.json` — pid, current `session_id`, model

## Scripts (`scripts/`)

A reference consumer: **voice-edit the in-progress prompt**. While dictating, hold
**Right Ctrl + Right Shift** to mark a recording as a *command*: a lean Claude agent
interprets the spoken instruction and edits the text you are currently composing in
the focused input field — edit, append, submit, or clear. Scope is deliberately
narrow: the agent only ever touches the unfinished prompt, never files, output, or
other panes. Field I/O is clipboard + ydotool, so it works in any focused field.

- `cmd-key-watcher.py` — read-only evdev watcher: Right Shift DOWN latches a
  one-shot `voxtype-cmd-mode` flag (does not grab the key — normal Shift still
  works). Bridges "Shift held at record time" to "flag read at transcribe time".
- `hey-claude.sh` — the handler: read the field (Ctrl+A/Ctrl+C → `wl-paste`) →
  `send --json` (CURRENT_PROMPT + INSTRUCTION) → parse → apply via ydotool. Reply
  contract is **JSON ops, fail-closed** (an unparseable reply does nothing):
  - `{"op":"replace","text":"…"}` — replace the whole field
  - `{"op":"append","text":"…"}` — add to the end
  - `{"op":"submit"}` / `{"op":"clear"}` / `{"op":"none"}`
- `rewrite-prompt.txt` — the agent contract (field-edit only); single source of
  truth, shared by the launcher and the systemd unit.
- `hey-claude-up.sh` — keep-warm. Default: idempotent detached launch. `--foreground`
  for systemd supervision. `CP_MODEL=claude-haiku-4-5-20251001` for fast warm turns.
- `zellij-field.sh` — (legacy) zellij-native pane input read/replace; superseded by
  the clipboard path in `hey-claude.sh` but kept for the zellij-only route.

Command-mode routing lives in voxtype's `llm-gate.sh` (a `~/.config` dotfile, not
this repo): if the `voxtype-cmd-mode` flag is fresh (or the transcript opens with
"hey claude" as a no-key fallback), the transcript is routed to `hey-claude.sh` and
nothing is pasted. Keep-warm is a systemd user service
(`~/.config/systemd/user/claude-pipe-voxtype.service`); the watcher should likewise
run as a user service so the flag is always latched.

## Status

First slice: core pipe (`up` / `send` / `down` / `status`), one model session,
text in / text out. Not yet: multi-message batching, an "attach to watch" pane,
structured-output (`--json`-schema) replies. The wire contract is stable.
