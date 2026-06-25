# claude-pipe v2 — ACP transport: implementation plan (gated against the spec)

> **Gated artifact.** Every item below cites the spec line it serves
> (`← §X`). The spec is the frozen ceiling: `docs/acp-transport-spec.md`
> (a.k.a. the plan-file spec "claude-pipe v2 — ACP transport for a
> model-as-client orchestrator"). Anything considered but lacking a spec parent
> is in the **Orphan ledger**, not the plan. The **Coverage check** lists any
> spec line with no plan item (under-build). This is the plan, not the build.

## Altitude note

The spec is mid-sized and unusually prescriptive (§11 already hands down a
delete/reuse/replace map; §12 hands down verification). So this plan is mostly
*sequencing + tracing* of work the spec already named, not new design. Where the
spec leaves a genuine choice, it says so explicitly (only **one**: the per-session
overflow policy, §6.3) — that is the single decision escalated below, not buried.

---

## Plan

### Phase 0 — Preserve v1 (non-destructive pivot)

1. **Add v2 as a parallel mode; do not delete v1's deployed behavior in place.**
   Build the new transport under new verbs/modules; keep v1 `up`/`send`/`down`/
   `status` working until v2 is verified. ← §11 ("v1 remains usable during the
   pause; v2 is a parallel mode, not a destructive rewrite") + §12.8 ("voxtype
   untouched … v1 mode intact throughout").

### Phase 1 — Per-agent data-path relay (the core)

2. **Replace the body of `daemon.rs::handle_conn` with two non-blocking,
   byte-faithful forwarding loops** (client-socket ⇄ agent-stdio), reading and
   writing raw bytes both directions simultaneously; neither direction may block
   the other. ← §3.1 (byte-faithful, two non-blocking loops) + §5 ("transport,
   not RPC … streams bytes both ways simultaneously") + §11 (REPLACE handle_conn).
3. **Keep `run_accept_loop` (accept + SIGINT/SIGTERM/child-exit select) as the
   shell around the new handler.** ← §11 (REUSE run_accept_loop).
4. **One data socket per agent process** at
   `$XDG_RUNTIME_DIR/claude-pipe/<agent_id>.acp.sock`; many ACP sessions
   multiplex over it. ← §5 (one data socket per agent; sessions multiplex) + §4
   (sessionId-routed multiplexing).
5. **claude-pipe owns each agent's stdin/stdout directly** (spawned child, piped
   fds); the relay bridges those fds to the accepted socket connection. The data
   path MUST NOT traverse any PTY/terminal-emulator stage. ← §3.3 + §5 ("owns
   each agent's stdin/stdout directly … never through a terminal pane") + §10.

### Phase 2 — sessionId framing + per-session fairness

6. **Parse exactly one field — `sessionId` — from frames, for routing only.** No
   other ACP semantics. ← §3.2 (semantics-blindness; MAY read only sessionId) +
   §2 ("parses exactly two things").
7. **Demultiplex agent→client frames by `sessionId` into per-session bounded
   queues; drain equally by default.** A slow/quiet session MUST NOT stall a busy
   one on the same connection. ← §6.3 (mechanical equal default; per-session
   queue) + §3.4 (default equal treatment).
8. **Layered overflow: continuous-drain → soft-bound backpressure+surface →
   hard-bound lease-teardown (OPEN-1 RESOLVED).** (a) keep **continuously
   draining** the agent's shared stdout into per-session queues — never halt the
   agent read (would stall all sessions); apply backpressure only on the
   *forward* side. (b) at a session's **soft bound**, stop forwarding that
   session onward, let its queue grow "pressured", and **loudly surface** it on
   telemetry (`queue_depth` pegged, `oldest_unread_ms` climbing) while other
   sessions flow. (c) only at a **hard memory bound** tear *that client's* lease
   with a logged reason. Drop-oldest is **forbidden** (violates §3.1 — blind
   transport can't tell a chunk from a load-bearing request). ← §6.3 (resolved
   layered overflow) + §3.1 (byte-faithful → no drops) + §3.5 (never-silent) +
   §3.4 (surface, orchestrator actuates).

9. **In-band-only sourcing (Invariant 9).** The relay reads/writes **only** the
   agent's stdio + claude-pipe's own sockets. No code path opens the filesystem,
   an agent's logs/transcripts, or a result artifact; no result/artifact
   convention exists. (Largely enforced *by exclusion* — there is no such code —
   but it is an explicit, testable property, §12.8, not just an absence.) ← §3.9
   (strictly in-band) + §2 (sources from only the agent's stdio).

### Phase 3 — Turn-open tracking (steal safety only)

10. **Track "is a turn open?" per session** by bracketing `session/prompt` →
    `stopReason`. This is the *only* second field of awareness permitted, and it
    exists solely to gate handoff safety (Phase 5). No request/response
    id-pairing, no replay. ← §2 ("turn-open/closed") + §9 (steal safety =
    turn-boundary only; "the only semantic peek … is knowing 'is a turn open?'").

### Phase 4 — Warm pool, recipe registry, spawn ownership

11. **Recipe registry**: a recipe declares spawn command + args, env/auth, pool
    size, idle policy, and a kind/name. claude-pipe owns spawning, keep-warm, and
    restart. ← §7 (recipe = spawn cmd/args/env/auth/pool/idle + kind/name) + §8
    (claude-pipe owns spawning/keep-warm/restart).
12. **`acp-stdio` recipe type** — spawn a first-party stdio ACP agent, pipe its
    stdio, expose on the data socket. (Primary; verify against `gemini --acp`.) ←
    §7.1.
13. **`claude-channels` recipe type** — keep a live `claude --channels` session +
    a small MCP server (`claude/channel` capability) bridging tasks/replies;
    carry the §7.2 caveats (research-preview; `--dangerously-load-development-
    channels`; keep-alive). Recipe MUST NOT contaminate `acp-stdio` data-path
    purity. ← §7.2.
14. **Pre-spawn + `initialize` agents to idle (warm)** so attach pays no cold
    start. ← §2.A + §7 (pre-spawned, initialize-d, idling).
15. **Persist pool state** (agent id, recipe, pid) via the reused atomic state
    I/O, so `list`/`attach` survive orchestrator restarts; detach from the
    launching shell via the reused setsid ceremony. ← §8 (persist pool state
    across restarts; detach via setsid) + §11 (REUSE spawn_detached/libc_setsid,
    write_state/with_suffix, State evolved to `{pid, recipe, …}`).

### Phase 5 — Single-client lease + turn-boundary steal

16. **Single-client exclusive lease per agent**; `attach` grants it. ← §9
    (single-client exclusive lease) + §3.6/§10 (no callback multiplexing → no
    fan-out).
17. **Active steal on contended attach**: a second attach forcibly hands off —
    old socket dropped, new client takes the lease. ← §9 (contention = active
    steal / forcible handoff).
18. **Steal permitted only at a turn boundary**: if a `session/prompt` is in
    flight for that agent, the handoff waits for `stopReason`; at idle it is
    immediate. ← §9 (steal safety = turn-boundary only; may briefly wait).

### Phase 6 — Control surface (out-of-band)

19. **Stateless control CLI, print-and-exit**: `list`, `attach <name|id>` (grants
    lease, **prints the data-socket path**), `spawn <recipe>`, `detach [<id>]`,
    `kill <id>`. ← §6.2 (verbs table) + §11 (CLI verb mapping: Up→spawn,
    Down→kill/detach, Status→list, new attach).
20. **Read-only telemetry stream** (`events --agent X`) emitting
    `{session, queue_depth, oldest_unread_ms, lifecycle}` lines; derived from the
    sessionId-framing (no ACP semantics). ← §6.2 (signals stream) + §11 (new
    `events --agent`) + §3.4 (surface).
21. **Control verbs and telemetry are out-of-band — never on the data socket;
    the data socket stays pure ACP** (stock-client-invisible). ← §3.7 + §6.1 (no
    envelope on the data path).

### Phase 7 — Deletions (remove the v1 half-duplex `-p` machinery)

22. **Delete the half-duplex turn machinery** once v2 verifies: `daemon.rs`
    `Job`, `worker_loop`, `TurnOutcome`, `run_one_turn`, `read_until_result`,
    `drain_pending_result`, `owes_result`, the `{"type":"user",…}` envelope build;
    `spawn_claude` (its lean/full + model knobs already informed the recipe
    format); `protocol.rs` `Request`/`Response`; `main.rs` `Send` +
    `client.rs::run_send`. ← §11 (DELETE list) + §6.1 (no ACP-path sugar → Send
    gone) + §10 (no blocking send on ACP path).
23. **Reuse, don't rewrite, the transport-agnostic infra**: `runtime_dir`,
    `socket_path` (→ per-agent `<id>.acp.sock`), `state_path`,
    `ensure_runtime_dir`, `libc_getuid`, `libc_kill`. ← §11 (REUSE list).

### Phase 8 — Verification (prove conformance)

24. **Implement the §12 verification suite as the done-gate** (now nine checks):
    (1) stock-client invisibility incl. byte-identical wire capture vs
    direct-spawn; (2) byte fidelity on an over-wide `session/update` chunk;
    (3) multi-session fairness under a stalled reader — soft-bound pressured +
    surfaced, hard-bound lease-torn, B/C never silently stalled; (4) warm-start
    latency vs cold spawn; (5) handoff safety mid-turn vs idle, no callback-id
    hang; (6) callback pass-through uninterpreted; (7) recipe coverage —
    `acp-stdio` vs `gemini --acp` and `claude-channels` vs live `claude
    --channels` on the subscription; (8) strictly-in-band (no fd to logs/
    transcripts/result files); (9) voxtype/v1 untouched. ← §12 (all nine).

---

## Orphan ledger

Items considered while planning and **cut** for having no spec parent (the spec's
ethos is "lean floor; extension over responsibility" — §3.8 — so these are
exactly the over-build temptations it forbids):

- **Recipe config-file format / DSL / schema language.** The spec says a recipe
  *declares* fields (§7) but mandates no file format, TOML/YAML/JSON choice, or
  schema tooling. CUT — implementation detail, not a spec requirement; pick the
  minimal thing at build time without spec ceremony.
- **Agent health-checks / liveness probes / auto-restart-on-crash policy.** §8
  says "keep-warm and restart," but no health-check protocol, backoff curve, or
  restart-storm guard is specified. CUT as designed machinery; restart is a bare
  capability, not a subsystem. (Note: §12.4 only measures warm-start latency, not
  restart behavior.)
- **Metrics/observability beyond the three named telemetry fields.** §6.2 names
  exactly `{session, queue_depth, oldest_unread_ms, lifecycle}`. Counters,
  histograms, Prometheus, structured tracing — all CUT (no parent).
- **Authentication / authorization on the control socket or data socket.** No
  spec line requires access control on either surface. CUT.
- **Configurable socket paths / runtime-dir overrides beyond what v1 already
  has.** §5 fixes the path shape; no configurability is requested. CUT.
- **Retry / reconnection logic for a dropped orchestrator connection.** Durability
  is delegated to ACP `session/load` (§9); the spec assigns reconnection to the
  *client* re-attaching, not to claude-pipe buffering or retrying. CUT.
- **A blocking `send`/`prompt` convenience or any turn-completion helper.**
  Explicitly forbidden (§6.1, §10). CUT.
- **Multi-client fan-out / callback routing.** Explicitly out of scope (§9, §10,
  §3.6). CUT.
- **zellij integration code (supervise/display).** §10 + Appendix A permit zellij
  to supervise/display processes, but the spec body assigns claude-pipe *no* work
  there ("zellij in the architecture: yes; zellij in the bytes: never"). No plan
  item — it is a permitted *external* composition, not claude-pipe scope. CUT
  from this plan (correctly).
- **`--full` mode equivalent (Claude's full agent loadout).** v1's `--full` was a
  `claude -p` knob; v2 has no agent-loadout concept (agents are external,
  recipe-defined). CUT — no parent; folds into recipe args if ever needed.

**Resolved (was escalated; now decided in-spec):**

- **OPEN-1 — per-session overflow policy → RESOLVED.** Now fixed in §6.3 as
  layered **continuous-drain → soft-bound backpressure+surface → hard-bound
  lease-teardown**, with drop-oldest forbidden (it would violate byte-faithful
  §3.1). Built by **item 8**. No longer an open decision. ← §6.3 (resolved).

---

## Coverage check (spec lines with NO plan item — under-build audit)

*(Re-run against the 9-invariant spec, after the Invariant-9 addition and the
OPEN-1 resolution. Item numbers reflect the renumbered plan.)*

Walking the spec's normative content (§2 floor, §3 invariants ×**9**, §4 ACP
facts, §5 architecture, §6 seam, §7 recipes, §8 pool, §9 lease, §10 non-goals,
§12 verification) against the plan:

- **§3.1–§3.9 (all nine invariants):** covered — 3.1→item 2/5/8, 3.2→6, 3.3→5,
  3.4→7/8/20, 3.5→8, 3.6→16, 3.7→21, 3.8→Orphan ledger (enforced by exclusion),
  **3.9→item 9** (in-band-only sourcing — the new invariant now has a build item
  + verification §12.8, not just exclusion).
- **§4 (ACP facts):** *constraints on behavior*, not build items; satisfied by
  items 2/4/6/10/18 (byte-faithful relay, sessionId routing, turn bracketing).
  "Concurrency inherited for free" (§4) is covered by item 2's non-blocking
  dual-loop — **no separate concurrency engine** is built (correct; an avoided
  orphan).
- **§5 architecture:** items 2/4/5.
- **§6.1 / §6.2 / §6.3:** items 21 / 19+20 / 7+8 (8 now carries the resolved
  layered-overflow policy).
- **§7.1 / §7.2, §8, §9:** items 12/13, 11/14/15, 16/17/18.
- **§10 non-goals:** covered by *exclusion* (Orphan ledger) + items 9/21/22
  (in-band-only, purity, delete sugar). The §10 "out-of-band substance" non-goal
  pairs with Invariant 9 → item 9 (transport sources only stdio) + Orphan ledger
  (no log/artifact reader built). Non-goals correctly generate no *other* build
  work.
- **§12.1–§12.9:** covered by item 24 (1:1, now nine checks incl. the new
  strictly-in-band test §12.8).
- **Appendix A:** evidence, not requirements — no plan items (correct).
- **Appendix B (this committed spec):** "commit the spec into the repo" → **DONE**
  (UB-1 resolved): the spec is now at `docs/acp-transport-spec.md`, this plan at
  `docs/acp-transport-impl-plan.md`. The stray plan-file note is housekeeping.

**Under-build findings:**

- **UB-1 — RESOLVED.** The frozen spec is committed to the repo
  (`docs/acp-transport-spec.md`); this plan's `← §X` references now point at it.
- No spec line is left uncovered. With Invariant 9 newly traced (item 9 + §12.8)
  and OPEN-1 resolved (item 8), the plan is the spec and nothing but the spec —
  no remaining open decisions, no orphans kept.

**Smell check:** 24 items for a spec with **9** invariants + a code-change map +
**9** verification criteria is proportionate, not bloated; nearly every item maps
1:1 to a §11/§12 line or a §6–§9 mechanism. No phantom phases. The single
genuinely-deferred parameter (OPEN-1) is now closed; nothing else was invented.
