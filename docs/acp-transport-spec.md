# claude-pipe v2 — ACP transport for a model-as-client orchestrator

**Specification — frozen.** Normative language: MUST / MUST NOT / SHOULD / MAY.
This document defines what claude-pipe must *be* (its fitness) to optimally
transport the Agent Client Protocol (ACP v1) for a model-as-client orchestrator.
It is the contract the implementation plan (`docs/acp-transport-impl-plan.md`) is
gated against; it is not itself the implementation plan. Verified findings that
justify the design are in Appendix A; the existing-code change map (for the
eventual build) is §11.

---

## 1. Context — why claude-pipe is being repurposed

claude-pipe v1 wraps `claude -p --output-format stream-json`: one long-lived
headless Claude over a Unix socket, turn-completion detected by Claude's own
`result` event. **That foundation is the named casualty of Anthropic's June 2026
billing change** — the Agent SDK, `claude -p`, and GitHub Actions are slated to
move off subscription limits onto a separate API-rate credit pool. The change was
*paused* hours before its June 15 rollout (Anthropic: "advance notice before any
future change"), so `-p` rides the subscription *for now*, on borrowed time.
Building more `-p`-dependent surface is building on a condemned foundation.

`claude -p` performs **no unique function** — it is merely the *headless drive
surface*. The asset is the **OAuth-authenticated Claude Code process**; `-p`, the
interactive TUI, and the IDE extensions are different *drive surfaces* on that
same process. The killswitch keys on the **surface** (headless/SDK), not on auth.

**The pivot:** claude-pipe stops *being* an agent and becomes a **transport**
between a **model-as-client orchestrator** — a programmatic drop-in for the human
who would otherwise sit in front of N interactive agent sessions — and a fleet of
**ACP-speaking agents**. claude-pipe bills nothing; it transports. Whatever does
or doesn't ride a subscription is the *far-end agent's* concern (chosen per
recipe, §7). **claude-pipe is billing-agnostic by being protocol-pure** — the one
layer that survives any billing change.

**Driving use case:** an orchestrator model holding many concurrent interactive
agent sessions (Claude, Gemini, Codex, …), spawning / prompting / reading /
steering / parking / resuming them through claude-pipe, exactly as a human
operator would tab between IDE chats — but programmatically.

---

## 2. What claude-pipe IS (the fitness floor)

claude-pipe is a **`sessionId`-aware, semantics-blind, full-duplex ACP conduit**
and a **supervisor of a warm, named, client-outliving pool of ACP-speaking
agents**. It exposes each agent's stdio ACP **byte-faithfully over a per-agent
Unix socket** (single-client exclusive lease; turn-boundary steal for handoff),
forwarding frames **per-session fair-by-default and never-silent**, through a
**stateless control CLI** (list / attach / spawn / detach / kill) and a
**read-only telemetry stream** (per-session depth / age / lifecycle).

It **surfaces** everything an orchestrator needs to prioritize and **decides**
nothing. The ACP data path **never** traverses a terminal emulator. Server-
initiated callbacks pass through **blind**. Reattach durability is **delegated to
ACP `session/load`**. It parses exactly two things — `sessionId` (routing /
fairness) and turn-open/closed (steal safety) — and is otherwise byte-transparent.

### The four reasons it exists (none removable without collapsing the design)

- **A — Warm pool.** Agents are pre-spawned, `initialize`-d, and idling. The
  orchestrator never pays ACP cold-start. *This is the latency story* — overhead
  is amortized at pool fill, not per call.
- **B — Client-outliving.** Agents survive orchestrator crash / redeploy /
  handoff; a successor reattaches to live sessions.
- **C — Naming / discovery.** The orchestrator requests agents by name/kind
  ("a claude", "the gemini one", "#7"), not by knowing how to spawn them.
- **D — Transport bridging.** The orchestrator reaches **stdio-only** ACP agents
  over a **Unix socket** because it is a separate (possibly remote/containerized)
  process that cannot fork-and-pipe-stdio. The socket is the seam decoupling
  client lifetime from agent lifetime — it makes A/B/C physically possible.

---

## 3. Non-negotiable invariants

1. **Byte-faithful data path.** Frames on a data socket MUST be relayed verbatim,
   both directions, with no transformation, re-framing, buffering that reorders,
   or terminal-emulation processing. The relay is two independent, mutually
   non-blocking forwarding loops (client-socket ⇄ agent-stdio).
2. **Semantics-blindness.** claude-pipe MUST NOT parse, interpret, or act on ACP
   method semantics. It MAY read **only** `sessionId` (for routing/fairness) and
   track **turn-open/closed** per session (for steal safety, §9). It MUST NOT
   distinguish `session/prompt` from `fs/read_text_file` or any other method.
3. **No terminal emulator in the data path.** ACP bytes MUST NOT pass through any
   PTY/grid/terminal-emulator stage (see Appendix A — zellij verdict).
4. **Surface, never schedule.** claude-pipe MUST expose per-session telemetry and
   MUST NOT reorder, throttle, or pick winners among sessions. Prioritization is
   the orchestrator's act (by how it drains), never the transport's. *This is the
   line between transport and scheduler.*
5. **Never-silent.** A frame MUST NOT be dropped without a **logged, telemetry-
   surfaced reason**. Silent loss is forbidden — the orchestrator cannot reason
   about data it isn't told was discarded.
6. **Callbacks pass through blind.** Server-initiated agent→client requests
   (`session/request_permission`, `fs/*`, `terminal/*`) are forwarded to the
   leased client and its response forwarded back. **Answering them is a different
   tool, out of scope** (§10).
7. **Stock-client invisibility.** A data socket MUST be pure ACP. An orchestrator
   pointing an off-the-shelf ACP client library at the socket MUST be unable to
   tell the agent is warm / pooled / shared. No claude-pipe envelope on the data
   path, ever.
8. **Lean by default, extension over responsibility.** claude-pipe owns process
   lifecycle, framing fidelity, addressing, and telemetry — and nothing else.
   Every capability that would make it interpret payloads, answer callbacks,
   schedule, or model a specific agent belongs in a *separate* tool or a *recipe*.
9. **Strictly in-band.** claude-pipe MUST source content from **only** the agent's
   stdio. It MUST NOT read the filesystem, an agent's own logs/transcripts, or any
   external artifact, and MUST NOT implement a result/artifact convention. Every
   byte the orchestrator sees — control *and* substance — rides the data socket.
   *Rationale:* sourcing substance out-of-band would force per-agent log/artifact-
   format knowledge (Claude JSONL ≠ codex rollout ≠ Gemini) — the adapter zoo that
   destroys minimality and universality. Strictly-in-band is what keeps claude-pipe
   lean and agent-agnostic; it also makes byte-fidelity (Invariant 1) correct-by-
   construction (a JSON-RPC frame cannot be *partially* delivered) and the
   per-session fairness machinery (§6.3) load-bearing rather than speculative
   (substance *can* stream in-band, so congestion is real). (Distinct from §6's
   "out-of-band" — that means control verbs not riding the *data socket*; this
   means substance not sourced from *anywhere but the agent's stdio*.)

---

## 4. ACP v1 facts the design is built on (verified from spec)

- **Transport:** JSON-RPC over **stdio**; client launches agent as subprocess;
  agent reads stdin, writes stdout. **Newline-delimited JSON; messages MUST NOT
  contain embedded newlines.** Agent MUST NOT write non-ACP bytes to stdout; MAY
  log to stderr. Sockets aren't ACP-defined, but "custom transports [are]
  permitted if they preserve the JSON-RPC message format + lifecycle" — a Unix
  socket carrying the identical newline-JSON stream qualifies.
- **Roles:** *Agent* = the AI subprocess; *Client* = the driver (here, the
  orchestrator).
- **Client→agent methods:** `initialize`, `authenticate`, `session/new`,
  `session/load`, `session/prompt`, `session/set_mode`, `logout`.
- **Agent→client (server-initiated, mid-turn):** `session/request_permission`,
  `fs/read_text_file`, `fs/write_text_file`, `terminal/create`,
  `terminal/output`, `terminal/release`, `terminal/wait_for_exit`,
  `terminal/kill`.
- **Notifications:** agent→client `session/update` (`agent_message_chunk`,
  `agent_thought_chunk`, `plan`, `tool_call`, `tool_call_update`,
  `usage_update`); client→agent `session/cancel`.
- **Turn model:** `session/prompt` does **not** return until the turn completes;
  the agent streams `session/update`, may issue server-initiated requests, then
  **responds to the original `session/prompt` with a `stopReason`** (`end_turn` |
  `max_tokens` | `max_turn_requests` | `refusal` | `cancelled`).
- **Sessions:** `sessionId`-addressed; "multiple independent interactions with
  the same Agent" → **many sessions multiplex over ONE connection.**
- **Concurrency:** full-duplex; JSON-RPC `id`s keep concurrent in-flight messages
  straight both directions. *claude-pipe inherits this concurrency for free by
  being a faithful full-duplex relay — it need not implement turn concurrency.*
- **Reattach:** `session/load` replays history **iff** the agent advertises the
  `loadSession` capability.

---

## 5. Architecture

```
 model-as-client orchestrator (separate / remote process)
   │  speaks RAW ACP (stock client lib)        ▲ reads telemetry (read-only)
   │  over per-agent Unix socket                │  `claude-pipe events --agent X`
   ▼                                            │
 ┌───────────────────────────────────────────────────────────────────────┐
 │ claude-pipe (supervisor + conduit)                                      │
 │                                                                         │
 │  control CLI (stateless, print-and-exit):                               │
 │    list · attach <name|id> → prints socket path + lease · spawn         │
 │    <recipe> · detach · kill <id>                                        │
 │                                                                         │
 │  per agent:  [data socket] ⇄  two non-blocking forwarding loops  ⇄      │
 │              [agent stdio]   (byte-faithful; reads sessionId only;      │
 │                               per-session fair queues; never-silent)   │
 │                                                                         │
 │  warm pool + recipe registry  ·  per-session telemetry counters         │
 └───────────────────────────────────────────────────────────────────────┘
   │ owns stdin/stdout of each agent process directly (NOT via any PTY/grid)
   ▼
 ACP agents (warm):  acp-stdio recipe (gemini --acp, codex, …)
                     claude-channels recipe (live `claude --channels` + bridge)
   (zellij MAY supervise/DISPLAY these processes — never carries their bytes)
```

- **One data socket per agent process** at e.g.
  `$XDG_RUNTIME_DIR/claude-pipe/<agent_id>.acp.sock`. Many ACP **sessions**
  multiplex over that one socket (ACP routes them by `sessionId`).
- claude-pipe **owns each agent's stdin/stdout directly** (spawned child, piped
  fds) — the architecturally decisive requirement: a subprocess whose stdout
  carries protocol bytes must be run *under the controller's own stdio*, never
  through a terminal pane.
- The relay is **transport, not RPC**: it does not read-one/write-one; it streams
  bytes both ways simultaneously, so the agent can stream `session/update` and
  fire callbacks while a `session/prompt` is still open.

---

## 6. The orchestrator↔claude-pipe seam (fully specified)

The seam is split: **raw ACP on the data path; everything else out-of-band.**

### 6.1 Data path — raw ACP, per-agent socket (S1)

- The per-agent Unix socket **IS** the agent's ACP stdio, byte-faithful, **no
  envelope** (Invariant 7). The orchestrator attaches a stock ACP client.
- Consequence — **no turn-completion sugar (S3):** there is no "block until
  `stopReason`" affordance; that would require an envelope. The orchestrator
  tracks `session/prompt`⇄`stopReason` itself (its ACP library already does).
  v1's blocking `send` ergonomic does **not** survive onto the ACP path.

### 6.2 Control surface — stateless CLI + read-only telemetry (S5)

Two parts, both **out-of-band** (never on the data socket):

**Verbs — stateless CLI, print-and-exit, scriptable:**

| Verb | Effect |
|---|---|
| `list` | enumerate agents (id, recipe/kind, lease holder, liveness) |
| `attach <name\|id>` | grant the lease; **print the data-socket path** to stdout |
| `spawn <recipe>` | start a new agent from a registry recipe (§7) |
| `detach [<id>]` | release the caller's lease |
| `kill <id>` | terminate a pooled agent (SIGTERM) |

**Signals — read-only telemetry stream** (e.g. `claude-pipe events --agent X`):
streams lines of `{session, queue_depth, oldest_unread_ms, lifecycle}`.
Continuous facts a print-and-exit CLI cannot push (backpressure, agent-death,
lease changes). **Derivable from the `sessionId`-framing already required** — no
ACP semantics needed.

### 6.3 Fairness — layered; surface, never schedule (S2′)

- **Default behavior:** mechanical **equal** per-session treatment — demultiplex
  agent→client frames by `sessionId`, hold a **per-session** bounded queue, drain
  fairly. A slow/quiet session MUST NOT stall a busy one on the same connection.
- **Information surface:** the telemetry stream (§6.2) exposes per-session depth /
  age / lifecycle.
- **Actuator:** the orchestrator's **drain pattern** — it reads the sockets/
  sessions it values first. claude-pipe never ranks value (it cannot know it).
- **Overflow = layered backpressure → teardown backstop (RESOLVED, OPEN-1).** The
  one parameter the spec previously deferred is now fixed. Drop-oldest is
  **forbidden** (it violates byte-faithful Invariant 1 — a semantics-blind
  transport cannot tell a droppable `session/update` chunk from a load-bearing
  agent→client *request*, so any drop risks corrupting the JSON-RPC stream / a
  hung `id`). The policy is:
  1. **claude-pipe MUST keep continuously draining the agent's shared stdout**
     into per-session queues (sessions are interleaved on one pipe; ceasing to
     read would stall *all* sessions → violates §6.3 mustn't-penalize-others and
     §3.4). Backpressure is applied **only on the forward side** (what is written
     onward to the client), never by halting the agent read.
  2. **Soft bound = surface, don't act.** When a session's forward queue reaches
     its soft bound, claude-pipe stops *forwarding that session's* frames onward
     and the queue is allowed to grow into a "pressured" state, **loudly surfaced**
     on telemetry (`queue_depth` pegged, `oldest_unread_ms` climbing). Other
     sessions are unaffected. The orchestrator, seeing this, drains the session it
     values (§3.4 actuator). No frame is altered or dropped.
  3. **Hard bound = lease teardown, never a mid-stream drop.** Only if a session's
     pressured queue exceeds a hard **memory** bound does claude-pipe tear **that
     client's lease** with a logged, telemetry-surfaced reason (Invariant 5). This
     bounds memory while remaining byte-faithful (a torn lease loses the *whole*
     stream cleanly — it never silently corrupts a live one) and per-session in
     trigger (one wedged session, not a transient slow one, causes it).
  This satisfies byte-faithful (1), per-session + mustn't-penalize-others (§6.3),
  never-silent (5), surface-not-schedule (4), and is memory-safe.

---

## 7. Agent recipes (the extension surface)

claude-pipe owns a **recipe registry** (§8 spawn-ownership). A recipe declares how
to bring up one kind of warm ACP agent: **spawn command + args, env/auth, pool
size, idle policy**, and a **kind/name** for discovery (C). Recipes are the *only*
place agent-specific or billing-specific knowledge lives — the core stays generic.
Two recipe **types** are in scope for v2:

### 7.1 `acp-stdio` — the pure case (primary)

A first-party stdio ACP agent: claude-pipe spawns it, pipes its stdio, exposes it
on the data socket. Confirmed real stdio ACP agents: **Gemini CLI (`gemini
--acp`), Codex, Cursor, Copilot CLI, Goose, OpenHands, Cline** (30+ in the ACP
registry). This is claude-pipe's clean, caveat-free identity proof.

### 7.2 `claude-channels` — subscription-safe Claude (strategic)

The **only** verified subscription-safe, non-`-p`, non-SDK way to prompt Claude as
an agent (Appendix A): a recipe that keeps a live **interactive `claude
--channels`** session and bridges tasks via a small **MCP server declaring the
`claude/channel` capability** — push `notifications/claude/channel` → Claude works
→ result returns via a `reply` tool; permission prompts relay. Runs the regular
interactive process against real files → subscription side of the billing line.

> **Caveats this recipe MUST carry (do not bury):** (1) **research preview** —
> `--channels` syntax/contract may change; the recipe is the blast-radius
> container for that churn. (2) custom channels need
> `--dangerously-load-development-channels`. (3) it steers an *already-running*
> session — the recipe MUST keep a `claude` alive (not fire-and-forget).
> **Architectural note:** this recipe is *not* a clean ACP agent — it bridges
> Channels↔ACP-ish flow. Whether it presents on the data socket as ACP or as a
> distinct protocol the orchestrator opts into is a recipe-level decision the
> plan must settle; it MUST NOT contaminate the `acp-stdio` data-path purity.

> **PoC DEMONSTRATED end-to-end on the subscription (2026-06-24).** A minimal
> two-way Node channel server (`server.mjs` + `.mcp.json`, MCP SDK 1.29.0) was
> launched via `claude --dangerously-load-development-channels server:probe`
> (`ANTHROPIC_API_KEY` unset → subscription OAuth; Claude Code v2.1.186, floor
> v2.1.80). A task pushed by `curl` (`localhost:8788`) arrived in the live session
> as `<channel source="probe" chat_id="1">`, Claude worked it, called the `reply`
> tool, and the result returned via the `/events` SSE stream — full task-in →
> agentic-work → result-out round-trip, no `-p`, no Agent SDK. The throwaway probe
> lives in this session's scratchpad (`…/scratchpad/channel-probe/`); the real
> recipe is impl-plan item #13. This is the concrete evidence behind A.1's
> "verified," and satisfies §11's `claude-channels` round-trip verification step.

---

## 8. Spawn ownership & the warm pool (S-spawn = supervisor)

A warm, named, client-outliving pool **requires** a supervisor that is not the
ephemeral client → **claude-pipe owns spawning, keep-warm, and restart.** It MAY
also *adopt* an already-running agent (attach to an existing side-channel socket),
but supervised spawning from a recipe is in scope. The supervisor persists pool
state (agent id, recipe, pid) so `list`/`attach` work across orchestrator
restarts, and detaches from the launching shell (reusing v1's setsid ceremony).

---

## 9. Lease & handoff model

- **Single-client exclusive lease per agent.** ACP server-initiated callbacks
  have **no client-multiplexing** (a callback addresses *the* client); routing one
  callback among several clients would require understanding callbacks → out of
  scope (Invariant 6). So one agent socket = one client at a time.
- **Contention = active STEAL (handoff-while-alive is wanted).** A second
  `attach` to a live-leased agent performs a **forcible handoff**: the old socket
  is dropped, the new client takes the lease. ("Refuse-if-live / adopt-if-dead"
  covers crash recovery but NOT live handoff, which is a required capability.)
- **Steal safety = turn-boundary only (leanest blind).** A live steal is
  permitted **only when no `session/prompt` is in flight** for that agent — no
  open turn ⇒ no orphanable server-initiated request ⇒ the agent is never left
  hanging on an unanswered JSON-RPC `id`. A handoff MAY briefly **wait** for the
  current turn's `stopReason`. The **only** semantic peek this costs is knowing
  "is a turn open?" per session — already bracketed by the `prompt`→`stopReason`
  pairing tracked via `sessionId`. No request/response id-pairing, no replay, no
  synthesized errors.
- **Durability = delegate to ACP `session/load`.** claude-pipe holds **zero**
  session memory for reattach history. On reattach the new client calls
  `session/load` and the **agent** replays. Full B requires pooled agents to
  advertise `loadSession`; agents lacking it still reattach but without
  history-replay (a recipe-level property to document, not a transport feature).

---

## 10. Explicit non-goals (preserve the floor)

- **Callback-answering / an ACP client runtime.** Forwarded blind; a separate
  tool may field `fs/*` / `request_permission` / `terminal/*`.
- **Out-of-band substance handling.** Reading result files, tailing agent logs,
  artifact conventions — the **orchestrator's** concern, one layer up (same
  discipline as callbacks). Strictly-in-band (Invariant 9) does **not** force all
  substance *through* the pipe: the orchestrator can route substance out-of-band
  whenever it wants — by *prompting* an agent to "do the work, write the result to
  a file, reply with just the path" — a per-work-packet choice that is invisible
  to and unowned by the transport. The control-plane / substance-plane hybrid (and
  any fire-and-forget vs tight-puppeteering mix) lives **above** claude-pipe, never
  inside it. The transport carries the control plane faithfully and stays agnostic
  to how substance is routed.
- **Scheduling / prioritization / value-ranking.** Surfaced, never enacted
  (Invariant 4).
- **Multi-client fan-out of one agent.** The lease is exclusive (§9).
- **Folding in MCP to "rescue" subscription-Claude.** `claude mcp serve` is
  tools-out, not agent-in (Appendix A) — it cannot serve the orchestrator. Not in
  scope.
- **zellij in the data path.** Four-ways falsified (Appendix A). zellij MAY only
  supervise/display agent processes; it MUST NOT carry ACP bytes.
- **Turn-level concurrency engine.** Unneeded — ACP's own `id`-matching provides
  it; a faithful full-duplex relay inherits it.
- **A blocking `send` on the ACP path.** Removed (§6.1).

---

## 11. Code-change map (for the eventual implementation — not this phase)

Existing core: 4 modules, ~932 lines, **cleanly separable** from the voxtype
`scripts/` consumer (no `src/`→`scripts/` code refs). **Repurposing the core does
NOT disturb the live voxtype dictation tool.** v1 remains usable during the pause;
v2 is a parallel mode, not a destructive rewrite of what's deployed.

- **DELETE (~275 lines) — half-duplex `claude -p` turn machinery:**
  `daemon.rs` `Job`, `worker_loop`, `TurnOutcome`, `run_one_turn`,
  `read_until_result`, `drain_pending_result`, `owes_result`, the
  `{"type":"user",…}` envelope build; `spawn_claude` (its lean/full + model knobs
  inform the recipe format); `protocol.rs` `Request`/`Response` (replaced by raw
  ACP on the data path); `main.rs` `Send` + `client.rs::run_send` (no ACP-path
  sugar, §6.1).
- **REUSE (~170 lines) — transport-agnostic infra:** `protocol.rs` `runtime_dir`,
  `socket_path` (→ per-agent `<id>.acp.sock`), `state_path`, `ensure_runtime_dir`,
  `libc_getuid`; `State` (evolve `{pid, session_id, model}` → `{pid, recipe,
  …}`); `daemon.rs` `spawn_detached` + `libc_setsid` (setsid ceremony),
  `run_accept_loop` (accept + signal select), `write_state` + `with_suffix`
  (atomic state I/O); `client.rs` `libc_kill`.
- **REPLACE the body of `daemon.rs::handle_conn`:** today read-one-line → worker →
  write-one-line; under §3 it becomes the **two non-blocking byte-faithful
  forwarding loops** (client-socket ⇄ agent-stdio). The accept loop survives; the
  handler body is new.
- **CLI verb mapping (`main.rs` `Cmd`):** `Up`→`spawn` (recipe-aware, keep
  `--detach`); `Send`→**deleted**; `Down`→`kill`/`detach`; `Status`→`list`; **new:
  ** `attach <name|id>`, `events --agent`.

---

## 12. Verification (how to prove an implementation meets this spec)

1. **Stock-client invisibility (Invariant 7):** point an off-the-shelf ACP client
   (e.g. Zed, or a minimal ACP client lib) at an `attach`-returned socket; it
   completes `initialize`/`session/new`/`session/prompt` and receives a
   `stopReason` with no awareness of pooling. Compare a wire capture to the same
   client driving the agent spawned directly — byte-identical ACP.
2. **Byte fidelity (Invariant 1):** push a `session/update` chunk larger than any
   plausible terminal width containing characters that a grid would wrap/escape;
   assert the orchestrator receives it byte-identical.
3. **Multi-session fairness (§6.3):** open ≥3 sessions on one agent; stall the
   reader of session A; assert sessions B/C continue to drain and that A's
   backlog is surfaced on telemetry — never a silent stall of B/C. Drive A past
   its soft bound (pressured + surfaced, B/C still flowing) and past its hard
   bound (A's lease torn with a logged reason; B/C unaffected). ← §6.3 layered
   overflow.
4. **Warm-start latency (A):** measure orchestrator-visible time from `attach` to
   first `session/new` ack against a pre-filled pool vs a cold spawn; warm path
   adds no cold-start.
5. **Handoff safety (§9):** attach client-1, start a turn, attempt a steal from
   client-2 mid-turn → handoff waits for `stopReason`; attempt a steal at idle →
   immediate. In neither case does the agent hang on an unanswered callback id.
6. **Callback pass-through (Invariant 6):** drive an agent that issues
   `fs/read_text_file`; assert claude-pipe forwards it to the leased client and
   relays the response, without interpreting it.
7. **Recipe coverage (§7):** bring up `acp-stdio` against a real stdio ACP agent
   (Gemini `--acp`) end-to-end; bring up `claude-channels` against a live
   `claude --channels` and confirm a task round-trips on the subscription.
8. **Strictly-in-band (Invariant 9):** assert claude-pipe opens no file descriptor
   to anything but the agent's stdio + its own sockets — no reading of agent logs,
   transcripts, or result files. (e.g. inspect open fds / strace under load.)
9. **voxtype untouched:** the existing dictation consumer continues to function
   (v1 mode intact) throughout.

---

## Appendix A — Verified findings (evidence the design rests on)

**A.1 Billing / Claude agent surfaces.**
- June 2026 split (Agent SDK / `-p` / Actions → separate API-rate pool) is
  **PAUSED, not cancelled** (halted hours before June 15; "advance notice before
  any future change"). Stay of execution.
- `claude -p` = headless drive surface, **no unique function**; killswitch keys on
  surface, not auth. A subscription-OAuth wrapper that drives via `-p`
  (harukitosa/claude-code-acp) still sits on the killswitch → rejected.
- Official `agentclientprotocol/claude-agent-acp` (Zed-built, Agent SDK) is
  **API-key-only**; ToS forbids subscription OAuth in third-party tools (issue
  #517).
- `claude mcp serve` is **tools-out, not agent-in** (verified incl. binary
  decompile): exposes granular tools for the *client's* model; no model loop, no
  cross-call context. MCP sampling flows server→client (wrong way) and is
  deprecated. **Cannot prompt subscription-Claude as an agent via MCP.**
- Other doors closed: IDE-extension WebSocket (Claude is the *client*; CVE-2025-
  52882), Agent SDK OAuth (officially NOT permitted; `CLAUDE_CODE_OAUTH_TOKEN` now
  meters usage), `--bare` (skips OAuth, API-key only), `remote-control`
  (subscription-safe but no inbound port / no third-party client API), hidden
  `--sdk-url` (undocumented, opaque billing).
- **Claude Code Channels** (research preview) is the one verified subscription-
  safe agent-in surface → §7.2. **Empirically demonstrated end-to-end on the
  subscription 2026-06-24** (working two-way Node probe; task round-tripped
  through a live `claude --channels` via the `reply` tool — see §7.2 PoC note).

**A.2 zellij is NOT in the data path — four independent angles, all agree.**
- **CLI:** `dump-screen`/`write-chars`/`paste` are grid/display operations.
- **Plugin API source** (vendored `zellij-tile` 0.44.3/0.43.1): no raw-subprocess-
  stdio relay. `run_command`→`RunCommandResult(exit, Vec<u8>, Vec<u8>, ctx)` is
  **one-shot** (fires after exit; no streaming, no stdin to a running process);
  `write`/`write_to_pane_id` is PTY **input injection**; `PaneUpdate`/
  `ReadPaneContents` is the **rendered grid** only; the `pipe` payload is a
  `String`. Permission enum has no "raw subprocess output capture."
- **Switchtail** (shipping fleet-driver plugin): tracks **metadata only**, reads
  **zero** pane output, withholds `ReadPaneContents`.
- **`zellij web`** (0.43+, upstream source): server→browser is whole-screen
  composited ANSI; per-pane `PaneRenderUpdate` is **disabled for web clients**;
  browser→server is parsed input events to the focused pane, per-session only.
- **The failure is asymmetric:** a faithful **write** into a pane's stdin exists
  (`write_to_pane_id(Vec<u8>)`); a faithful **read** of a pane's stdout does
  **not** (grid only, ANSI-interpreted, width-reflowed). Full-duplex byte
  protocols need both → zellij cannot carry ACP.
- **Pipe nuance:** the `zellij pipe` channel (CLI↔**plugin**) *does* carry
  newline-delimited JSON faithfully (UTF-8, newline-framed, no emulation on the
  pipe) — but it reaches a *plugin*, not a *pane's subprocess stdio*. The blocker
  is the pane-process read side, not the pipe.
- **Prior art confirms the ceiling:** `zjctl`/`zrpc` (NDJSON over a zellij pipe;
  writes via `write_chars_to_pane_id`; **reads via `dump-screen` grid**) and
  `zellij-agent-tools`/`zellij-mcp-sidecar` (MCP-over-stdio; **reads via
  `PaneRenderReport` ANSI-stripped grid; no write**) both hit the grid wall.
- **Decisive corollary:** "a subprocess that must own its stdout for protocol
  bytes must run *under the controller's own stdio* (or a dedicated socket/fd),
  not through a zellij pane" — which **is** claude-pipe's design.
- **Permitted zellij role:** supervise/display agent **processes** (persistent,
  named, reattachable, human-visible) while claude-pipe owns their stdio off-grid.
  zellij in the architecture: yes. zellij in the bytes: never.

---

## Appendix B — provenance

- This spec was derived in session and frozen here as the repo's committed
  contract. Its gated implementation plan is `docs/acp-transport-impl-plan.md`.
- The earlier "what should claude-pipe add to serve Turnbridge/Switchtail" record
  is `docs/serving-turnbridge-switchtail.md` (superseded in framing by this spec's
  pivot, but retained for the cross-project analysis).
