# Serving Turnbridge & Switchtail from claude-pipe — suggestions record

> **Status: suggestions only. Nothing here is implemented.** Record of an
> analysis (2026-06-23) comparing claude-pipe against two sibling labs
> (`~/JangLabs/turnbridge`, `~/JangLabs/switchtail`) to determine what — if
> anything — claude-pipe should add to serve them *without* betraying its
> design axis: **absolutely minimal, low-latency, fast by default, a thin
> reusable platform whose only required surface is `claude-pipe send`.**

## The shared problem (why these three labs touch)

All three solve one slice of the same problem: **get a prompt into an agent's
input, and know when it answered.** They solve it three ways, with zero shared
code:

| Capability | Turnbridge | Switchtail | claude-pipe |
|---|---|---|---|
| Own the agent process (read its `result` sentinel) | ❌ foreign TUIs | ❌ foreign panes | ✅ its whole point |
| Deliver text *into* a foreign TUI | `tmux load-buffer`→`paste-buffer -p`→`send-keys Enter` | `zellij pipe {"op":"say",…}` | ⚠ only in `hey-claude.sh` (zellij `write`/`write-chars`), not a core primitive |
| Detect a *foreign* agent's turn completion | poll rollout JSONL for `event_msg:task_complete` past a byte cursor | ❌ operator-driven | ❌ owns-process `result` only |
| Address a specific pane/line by id | hardcoded panes | `line:"terminal_3"` | ⚠ resolves the one focused pane via `list-clients` |

Grounding facts from the actual code:
- **Turnbridge `tb-await-verdict.sh`** hand-rolls turn detection by polling
  codex's rollout JSONL for `event_msg:task_complete` past a saved byte cursor —
  because it does not own the agent. claude-pipe's `read_until_result`
  (`daemon.rs`) is the *superior* version of this, but only for an agent it owns.
- **Turnbridge `tb-poke-codex.sh`** delivers via the tmux paste idiom above.
  **Switchtail** delivers via `zellij pipe {"op":"say",…}` through its plugin.
  Both are *weaker* than `hey-claude.sh`'s zellij-native `write-chars` +
  `\`+Enter soft-newlines + `list-clients` id-0-collision-proof pane resolution.

## What the claude-pipe Rust core does today (the baseline these build on)

932 lines across `protocol.rs` (105) / `daemon.rs` (564) / `client.rs` (139) /
`main.rs` (124). Exact function: spawn **one** long-lived
`claude -p --verbose --input-format stream-json --output-format stream-json`,
own its stdin/stdout directly (no multiplexer), and per request write a
`{"type":"user",…}` envelope to stdin and read stdout events until claude's own
`result` event — the turn sentinel. Serializes turns through one mpsc worker
(streaming stdin/stdout is one ordered channel). Lean by default strips
tools/MCP/settings/hooks and replaces the system prompt (cache tokens
~8.7k/~17.7k → 0). Recovers from a timed-out turn's pending `result` via
`owes_result`/`drain_pending_result` so replies never go off-by-one. Wire
contract: `Request{text,timeout_ms}` → `Response{ok,text,session_id,turn_ms,
error}`. It knows nothing about terminals, panes, foreign agents, or fleets —
and that omission is the design, not a gap.

## Suggestions, by tier

### Tier 1 — would add (tiny, pure-read, zero hot-path cost)

1. **`status --json`** emitting
   `{session, live, pid, session_id, model, socket, transcript_path}`.
   Today `status` (`client.rs`) prints human-readable text only and signals
   liveness via exit code. Both consumers want one machine-readable handle in a
   single call instead of scraping `<session>.state.json`. (~10 lines.)

2. **`transcript_path`** — resolve the on-disk JSONL of the agent claude-pipe
   *owns*, exposed via `status --json` and/or a `path` subcommand. This is
   exactly what Turnbridge's `tb-pin-codex.sh` does by hand to find a rollout to
   poll; claude-pipe already captures `session_id` (`State`), so it is a path
   computation, pure read. (~15 lines.) Lets a Turnbridge-style consumer *find
   and watch* an agent claude-pipe owns — and watch it via claude-pipe's own
   `result` detection, which beats polling JSONL.

### Tier 2 — would add as a SIBLING SCRIPT, never in the Rust binary

3. **Extract `hey-claude.sh`'s zellij-native pane delivery** (the handoff §8/§8a
   mechanism: `write-chars`, `\`+Enter soft-newlines between lines, Ctrl-C
   clear, `❯`+NBSP-aware read, `list-clients` pane resolution that dodges the
   plugin/terminal `id:0` collision) into a reusable
   `scripts/cp-deliver.sh <pane_id> <text>`. This is the crown jewel both
   Turnbridge and Switchtail are reinventing worse.
   **Constraint: it stays at script level.** It must NOT enter `daemon.rs` —
   "no terminal multiplexer in the hot path" is the binary's load-bearing
   invariant. claude-pipe-the-binary stays process-owning; pane delivery lives
   beside it, the way `hey-claude.sh` already does.

### Tier 3 — would explicitly REJECT (each turns claude-pipe into one of the others)

- ❌ **Generic "attach to a *foreign* agent and poll its rollout for
  completion."** That models foreign-agent turn detection (rollout formats,
  `task_complete` vs `result`, per-tool log locations) — exactly the
  multiplexer-shaped complexity the README refuses. It is Turnbridge's reason to
  exist; keep it there.
- ❌ **Multi-pane/line registries + a call log / directory.** That is a fleet
  controller — it is Switchtail. claude-pipe is a pipe to *one* agent.
- ❌ **A `say`-into-arbitrary-pane *daemon op*.** Tempting (it would unify
  Switchtail's `{"op":"say"}`), but it drags zellij into the binary. Script only.
- ❌ **Async/non-blocking sends, batching, flag-tiers, permission-gated
  enforcement.** All Turnbridge concerns; all violate "one turn at a time,
  blocks for reply."

## Bottom line

The only additions that fit the invariant are **read-only handles** (`status
--json` + `transcript_path`) so a Turnbridge-style consumer can find/watch an
owned agent, and a **script-level extraction** of the pane-delivery primitive
both others reinvent. Everything that would require modeling foreign agents or
fleets stays out — that is what keeps claude-pipe minimal and fast by default.
