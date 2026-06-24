# channels-kit — implementation plan (gated against spec §7.2 + channels-reference)

> **Gated artifact.** Every item cites the governing source it serves — either a
> line of `docs/acp-transport-spec.md` **§7.2** (the claude-channels recipe
> section, frozen + hardened) or a **channels-reference** requirement
> (<https://code.claude.com/docs/en/channels-reference>, verified 2026-06-24 on
> Claude Code v2.1.186). Anything considered but lacking such a parent is in the
> **Orphan ledger**. The **Coverage check** lists any §7.2 obligation with no plan
> item (under-build). The full research grounding is the session scratchpad
> `channels-parity-research.md`.

## What this is

A **standalone, reusable framework** (`channels-kit/`, Node) that exposes a live
`claude --channels` session as an **ACP-subset agent**, pushing toward ACP parity
on every surface the channel mechanism can actually carry, and **honestly
declaring** the surfaces it cannot. The existing `scripts/claude-channels-bridge.mjs`
becomes a thin consumer of this kit (claude-pipe's `claude-channels` recipe →
unchanged data-socket contract).

The **governing decision §7.2 demands the plan settle** ("Whether it presents on
the data socket as ACP or as a distinct protocol … is a recipe-level decision the
plan must settle; it MUST NOT contaminate the `acp-stdio` data-path purity"):
**channels-kit presents the ACP subset** (so the same stock ACP client + relay
drive it), as a **separate binary** the recipe spawns — never touching the
acp-stdio data-path code. ← §7.2 architectural note.

---

## Plan

### Phase 0 — Package skeleton + parity map (the honest contract)

1. **Create `channels-kit/` as a standalone Node package** (`package.json`,
   `index.mjs` public API `createChannelAgent()`, deps `@modelcontextprotocol/sdk`
   + `node-pty`). Reusable independent of claude-pipe. ← §7.2 ("a small MCP server
   declaring the `claude/channel` capability") + the §7.2 architectural note
   (separate, non-contaminating).
2. **Ship `channels-kit/PARITY.md`** — the per-method ACP-parity map
   (CAN/PARTIAL/CANNOT) with the channel mechanism behind each verdict, and the
   research-preview + permission-gating caveats inline. ← §7.2 caveats ("do not
   bury") + the §7.2/§9 framing of the CANNOT set as a documented recipe-level
   limitation, not a bug.

### Phase 1 — The channel server (Claude's MCP/stdio child)

3. **`channel-server.mjs`** — declare `experimental['claude/channel']:{}` +
   `tools:{}`; connect a `StdioServerTransport`; expose a **configurable tool
   surface** Claude calls back through. ← channels-reference (capability key +
   tools + server-defined reply tool) + §7.2 (`reply` tool).
4. **Multi-tool reply surface** (`say`, `think`, `finish`) replacing the single
   `reply`, with `chat_id`-keyed args + a strong `instructions` string steering
   Claude to stream via `say`/`think` and close via `finish`. (Server-named,
   multi-call replies are **live-verified**.) ← channels-reference ("the tool name
   is server-chosen … steered by instructions"; multiple tools allowed).
5. **Push tasks** via `notifications/claude/channel` `{content, meta:{chat_id}}`,
   meta keys constrained to `[A-Za-z0-9_]`. ← channels-reference (push method +
   meta key constraint).

### Phase 2 — Permission relay (the §7.2 requirement, currently missing)

6. **Declare `experimental['claude/channel/permission']:{}`** and handle inbound
   `notifications/claude/channel/permission_request` `{request_id, tool_name,
   description, input_preview}` via `setNotificationHandler`. ← §7.2 ("permission
   prompts relay") + channels-reference (permission relay, v2.1.81+).
7. **Surface each permission request to the ACP layer** as a
   `session/request_permission`-shaped event and **send the verdict** back via
   `notifications/claude/channel/permission` `{request_id, behavior:'allow'|'deny'}`.
   Default policy is pluggable (auto-allow / auto-deny / delegate-to-orchestrator).
   ← §7.2 (relay) + channels-reference (verdict method + first-answer-wins + sender
   gating caveat).
8. **Carry the permission-relay caveats** (only Bash/Write/Edit relay;
   project-trust + MCP-consent do NOT → still need the PTY auto-answer; gate
   senders since anyone who can reply can approve). ← channels-reference (relay
   scope + security note).

### Phase 3 — Claude lifecycle (the keep-alive liveness device)

9. **`lifecycle.mjs`** — spawn `claude --dangerously-load-development-channels
   server:<name> --mcp-config <cfg>` under a **node-pty PTY** (Claude needs a TTY
   or it drops to `-p` and errors); auto-confirm the one-time "local development"
   prompt (matcher + timer fallback); keep Claude **alive** for the agent's
   lifetime; clean teardown. The PTY carries only the discarded TUI — task/reply
   DATA never traverses it (Invariant 3). ← §7.2 caveat 3 ("keep a `claude`
   alive") + channels-reference (interactive/TTY requirement) + spec Invariant 3.
10. **Subscription posture**: `ANTHROPIC_API_KEY` unset (OAuth); document the
    `channelsEnabled`/`allowedChannelPlugins` org gates + the dev-flag confirmation
    + version floor (v2.1.80; permission v2.1.81). ← §7.2 ("subscription side of
    the billing line") + channels-reference (auth + org gating + floor).

### Phase 4 — Pluggable transports (serve relay AND standalone)

11. **`transports.mjs`** — a transport interface carrying `{push task}` /
    `{reply/stream/permission events}` between the ACP facade and the channel
    server: **unix-socket** (the internal seam, as the current bridge uses) +
    **http** (standalone `/push` + `/events` SSE, as the proven probe uses). ←
    §7.2 (the bridge ferries tasks/replies) + channels-reference (the probe's
    HTTP push/events pattern). *(Pluggability is an implementation choice, not a
    spec mandate — see Orphan ledger note.)*

### Phase 5 — The ACP-subset facade (what the recipe/orchestrator sees)

12. **`acp-facade.mjs`** — speak the ACP subset on stdio: `initialize` (advertise
    `loadSession:false`, text-only `promptCapabilities` — **honest**),
    `session/new` (mint id ⇄ chat_id), `session/prompt` (push → stream `say`/`think`
    as `agent_message_chunk`/`agent_thought_chunk` → `finish` closes with
    `stopReason:end_turn`), `session/cancel` (best-effort, documented), and
    **graceful stubs** for `authenticate`/`logout`/`session/load`/`session/set_mode`
    that don't hang a stock client. ← §4 (ACP method set) + §7.2 (Channels↔ACP
    bridge) + the parity map (item 2).
13. **Streaming turn**: map repeated `say` calls → multiple `agent_message_chunk`
    notifications (real intra-turn streaming), `think` → `agent_thought_chunk`,
    `finish` → final chunk + the prompt's `stopReason`. ← channels-reference
    (multi-call server-named tools) + §4 (session/update variants).
14. **Honest degradation**: ContentBlock[]→text flattening, hardcoded
    `end_turn`, and the absent `tool_call`/`plan`/`usage` telemetry are recorded
    in PARITY.md and logged once at startup, not silently swallowed. ← §7.2 caveats
    ("do not bury") + spec Invariant 5 ethos (never-silent).

### Phase 6 — Rewire the existing bridge + recipe onto channels-kit

15. **Refactor `scripts/claude-channels-bridge.mjs` to consume channels-kit**
    (thin wrapper: `createChannelAgent({ transport:'unix-socket', stdio:'acp' })`),
    preserving the exact data-socket contract claude-pipe's relay already speaks
    (so §12.7b stays green). The recipe (`recipe.rs`) is unchanged. ← §7.2 ("the
    real recipe is impl-plan item #13") + spec §12.7b (round-trip must still pass).

### Phase 7 — Verification (the done-gate)

16. **Unit/contract tests** (`channels-kit/test/`): the ACP-facade method mapping
    (mock channel server — no live Claude), the permission-verdict correlation
    (request_id matching), the multi-call streaming mapping, and the meta-key
    sanitizer. ← items 6/7/12/13.
17. **Live round-trip** (gated, subscription): drive a real `claude --channels`
    task through channels-kit standalone (HTTP) AND through claude-pipe's recipe
    (relay) — "17×23 → 391" — and exercise a permission relay (a Bash approval
    round-trips). ← §7.2 PoC + §12.7b + the permission-relay obligation.
18. **claude-pipe §12 suite still green** — `RUN_CLAUDE=1 bash tests/verify.sh`
    check7b passes through the refactored bridge. ← §12.7b + §12.9 (don't break
    what's proven).

---

## Orphan ledger (considered, CUT for no §7.2/channels-reference parent)

- **Server→client `createMessage`/`elicitInput`/`listRoots` to ask Claude mid-task.**
  Researched + **live-dead-ended**: Claude declares none of `sampling`/`elicitation`/
  `roots` over a channel, so these throw at the MCP capability gate. CUT — not a
  spec gap, an impossibility on this surface. (Permission relay uses the proprietary
  notification pair instead — item 6/7.)
- **`session/load` history replay / `session/set_mode` real implementation.** No
  channel primitive exists. CUT to stubs + PARITY.md CANNOT entries (items 2/12).
- **`tool_call`/`plan`/`usage_update` synthesis.** Claude's intra-turn telemetry
  never crosses the channel; nothing to derive them from. CUT (PARITY.md CANNOT).
- **Real per-session `session/cancel`.** Channels has no cancel; only whole-process
  kill exists and that forfeits all sessions on the agent (not per-session
  faithful). CUT to documented best-effort no-op (item 12 + PARITY.md PARTIAL).
- **Multi-chat_id concurrency in ONE session.** chat_id is not a routing primitive;
  pushes serialize. True concurrency = one live claude per session. CUT as a
  concurrency model change; channels-kit maps 1 session ⇄ 1 chat_id and documents
  the serialization (PARITY.md). *(A future "session-per-process pool" is a
  claude-pipe supervisor concern, not channels-kit's.)*
- **Structured/image/resource reply content + `edit_message`-style overwrite.** The
  channel mechanism supports it, but ACP's text-chunk streaming is the parity
  target; richer content blocks have no §7.2 obligation. CUT from the scaffold
  (the tool surface is extensible if a future need lands). Noted, not built.
- **`--channels plugin:<name>@<marketplace>` (allowlist/prod) packaging.** §7.2 +
  the verified path use the dev flag; publishing channels-kit as an allowlisted
  plugin is a distribution step with no §7.2 parent. CUT.
- **Transport pluggability beyond unix-socket+http.** Only those two have a
  grounded use (relay seam + standalone probe). More is speculative. CUT.

## Coverage check (§7.2 obligations → plan item)

Walking §7.2's normative content + the channels-reference contract:

- §7.2 "live interactive `claude --channels` session … keep a `claude` alive" → **item 9** (lifecycle/PTY/keepalive).
- §7.2 "MCP server declaring the `claude/channel` capability" → **items 3/5**.
- §7.2 "push `notifications/claude/channel` → Claude works → result via a `reply` tool" → **items 4/5/12/13**.
- §7.2 "**permission prompts relay**" → **items 6/7/8** (the previously-missing piece, now covered).
- §7.2 caveats (research preview / `--dangerously-load-development-channels` / keep-alive), "do not bury" → **items 2/8/10/14** (PARITY.md + inline + startup log).
- §7.2 architectural note (present as ACP **or** distinct opt-in; MUST NOT contaminate acp-stdio purity) → **settled: ACP subset, separate binary** (Phase 0 framing + item 15; channels-kit never imports acp-stdio data-path code).
- §7.2 "subscription side of the billing line" (ANTHROPIC_API_KEY unset) → **item 10**.
- §12.7b round-trip must stay green → **items 15/17/18**.
- channels-reference: meta-key constraint → **item 5**; permission relay wire → **items 6/7**; TTY requirement → **item 9**; org gates/floor → **item 10**.

**Under-build findings:** none. Every §7.2 obligation maps to a plan item; the
single architectural decision §7.2 escalates is settled (ACP subset, separate
binary). The CANNOT set is covered by exclusion + PARITY.md, exactly as §7.2/§9
frame it.

**Smell check:** 18 items for §7.2 + a verified channels-reference contract is
proportionate. The big additions over the current bridge — permission relay,
streaming, honest capabilities, reusable packaging — each trace to a §7.2 line or
a channels-reference requirement. No phantom phases; the genuinely-impossible
(server→client MCP requests) is correctly in the Orphan ledger, not the plan.

---

## Gate outcome (gate-plan skill, independent pass)

Gated against §7.2 + channels-reference. **Passed**, with three over-builds
honestly surfaced (they exceed the frozen §7.2 floor but are kept under the
user's explicit "as much ACP parity as you can confidently build" + "fully
fleshed framework" directive — they are flagged as **beyond-floor** in
`channels-kit/PARITY.md` so the boundary stays legible):

- **Streaming (item 13) + the multi-tool vehicle (item 4: `say`/`think`).** §7.2
  says "result returns via a **`reply` tool**" (singular, one-shot). The single
  `finish` tool alone satisfies §7.2; `say`/`think` + repeated-call streaming map
  to real ACP `session/update` variants (§4) and are live-verified, but §7.2
  imposes no streaming obligation. **Beyond-floor, kept (cheap + ACP-truer).**
- **HTTP transport (item 11).** The unix-socket seam alone satisfies §7.2's
  "bridges tasks". HTTP serves *standalone* use — a **user-parented**, not
  §7.2-parented, requirement. **Kept under the framework directive.**

The previously-missing **permission relay** (§7.2 "permission prompts relay") is
the correctly-identified under-build and is now the plan's spine (items 6/7/8).
No §7.2 obligation is left uncovered. The one decision §7.2 escalates
(ACP-vs-distinct data-socket presentation) is settled: **ACP subset, separate
binary, never importing acp-stdio data-path code.**
