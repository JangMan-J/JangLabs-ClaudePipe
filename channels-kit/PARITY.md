# channels-kit — ACP parity map

**What this is.** channels-kit exposes a live, subscription-authenticated
`claude --channels` session as an **ACP-subset agent**. ACP (the Agent Client
Protocol) is the full protocol claude-pipe's `acp-stdio` agents (e.g. `gemini
--acp`) speak. The **channel mechanism is a narrow, asymmetric pipe** (text task
*in* via a notification; tool calls *out*), so a channels-backed agent cannot
reach full ACP parity. This document is the **honest map** of what it can, what
it can partially, and what it cannot — per ACP surface — with the channel
mechanism behind each verdict.

Two boundaries are marked throughout:
- **[§7.2]** — required by the frozen spec `docs/acp-transport-spec.md` §7.2 (the
  `claude-channels` recipe contract). channels-kit MUST do these.
- **[beyond-floor]** — exceeds §7.2 (which only requires a one-shot `reply`), kept
  because it is ACP-truer and cheap, under the directive "as much parity as you
  can confidently build". Not a §7.2 obligation.

Verified against **Claude Code v2.1.186** (2026-06-24). Channels are a **research
preview** — the contract may change; channels-kit is the blast-radius container
for that churn, and behavior is pinned to the running Claude Code version.

---

## The map

| ACP surface | Verdict | Mechanism / why |
|---|---|---|
| `initialize` | **CAN** | Advertise `loadSession:false` + text-only `promptCapabilities` — an honest self-description of what the channel carries. |
| `authenticate` / `logout` | **CAN** (vacuous) | Auth is the live process's subscription OAuth (`ANTHROPIC_API_KEY` unset); there is no per-session ACP auth. Accepted as no-ops without misleading the client. |
| `session/new` → `sessionId` | **CAN** | Mint a sessionId, mapped 1:1 to a channel `chat_id`. |
| `session/new` `cwd`, `mcpServers` | **PARTIAL** | NOT honored — `claude` is spawned once at agent start with a fixed cwd and MCP config. Per-session cwd/MCP wiring is silently not applied. (True per-session isolation needs one `claude` per session — a claude-pipe supervisor concern, not channels-kit's.) |
| `session/prompt` → `stopReason` | **CAN** (core) | The real round-trip: prompt → channel push → Claude does agentic work on the subscription → `stopReason`. This is the load-bearing capability (spec §7.2 / §12.7b). |
| `session/prompt` `ContentBlock[]` | **PARTIAL** | Flattened to text. `image`/`audio`/`resource`/`resource_link` blocks are dropped (the channel carries a text body). `promptCapabilities` advertises text-only so a conformant client won't send them. |
| `session/update` `agent_message_chunk` | **CAN** | The reply text is emitted as message chunks. **[beyond-floor]** With the `say` tool, multiple chunks stream during the turn (live-verified: Claude calls a server-named tool repeatedly), not one terminal blob. |
| `session/update` `agent_thought_chunk` | **PARTIAL** **[beyond-floor]** | A `think` tool the framework offers maps to thought chunks. This is a server convention steered by instructions, not Claude's true reasoning stream — Claude chooses what to put there. |
| `session/update` `tool_call` / `tool_call_update` | **CANNOT** | Claude's own tool invocations run *inside* the live process and never cross the channel. No primitive to derive them. |
| `session/update` `plan` | **CANNOT** | Same — Claude's planning is intra-process, not channel-visible. |
| `session/update` `usage_update` | **CANNOT** | Token/context accounting is intra-process; the channel surfaces none of it. |
| `session/request_permission` | **CAN** **[§7.2]** (delegate mode) | Two layers. **Channel layer (always):** the documented `claude/channel/permission` relay (Claude Code ≥ v2.1.81) — Claude Code emits `notifications/claude/channel/permission_request` `{request_id, tool_name, description, input_preview}`; channels-kit answers `notifications/claude/channel/permission` `{request_id, behavior}`. **ACP layer (policy `delegate` only):** the facade emits a **real ACP `session/request_permission` request** (`{sessionId, toolCall, options:[allow_once, reject_once]}`) to the ACP client and maps the client's response — the flattened ACP v1 `outcome.optionId` (with `outcome.outcome` as the `selected`/`cancelled` discriminator) — back to the channel verdict, so a stock ACP client genuinely participates. **Delegate FAILS CLOSED:** the verdict is `allow` only on an explicit `allow_once`; reject, cancelled, a JSON-RPC error response, an unknown option, or a 30 s no-answer all map to **deny**. (When no ACP turn is open to attribute the request to, the facade does **not** fabricate a sessionId — it defers to the host callback, else denies.) In policy `allow`/`deny` (the recipe default for unattended use) the verdict is decided server-side and **no** ACP request is emitted. **Caveats:** relays Bash/Write/Edit tool approvals only — project-trust + MCP-consent dialogs do NOT relay (the PTY auto-answers those); the terminal dialog stays open in parallel and first-answer-wins; the relay only fires when Claude runs in a *prompting* permission mode (pass `permissionMode:'default'` — the box may default to `bypassPermissions`, in which case nothing prompts; the recipe/bridge set this to `default` automatically when you select `deny`/`delegate`); in `allow` mode anyone who can push to the channel can approve tool use, so **gate senders**. |
| `session/cancel` | **PARTIAL** (best-effort) | The channel has no cancel primitive, so Claude's in-flight work **cannot actually be aborted**. But the facade honors the ACP contract on the *response*: a `session/cancel` marks the turn, and the turn then closes with the spec-mandated **`stopReason: cancelled`** (ACP v1 requires `cancelled` after a `session/cancel`), via either Claude's eventual `finish` or a shortened 5 s timer — not a misleading `end_turn`. An id-bearing `session/cancel` is acknowledged. What it can't do: stop the underlying compute (only a whole-process kill would, which forfeits all sessions on that agent). |
| `session/load` (history replay) | **CANNOT** | The channel has no history-export/replay primitive. `loadSession:false` is advertised; a reattaching successor gets no replay (spec §9 documents this as a recipe-level limitation — full client-outliving durability is unavailable on this recipe). |
| `session/set_mode` | **CANNOT** | The channel exposes no mode-switching primitive to the spawned Claude. The client's `modeId` is accepted but has no effect. |
| `fs/read_text_file`, `fs/write_text_file` | **CANNOT** | ACP agent→client callbacks. The live Claude resolves file access *inside* its own process against real files; it never becomes an ACP callback on the data socket. (For a pure `acp-stdio` agent these DO pass through faithfully — the gap is specific to channels.) |
| `terminal/*` | **CANNOT** | Same as `fs/*` — Claude's terminals are intra-process; no channel equivalent. (ACP spec §10 frames client-mediated callbacks as forwarded-blind anyway.) |

---

## Concurrency

`chat_id` is **not** an ACP-routing primitive in the channel contract — it is a
`meta` attribute channels-kit invents. Multiple `chat_id`s pushed into **one**
session collapse into one serialized Claude context ("events queue and are
processed in order … to process independent streams concurrently, run separate
sessions"). channels-kit maps 1 ACP session ⇄ 1 `chat_id` over one live Claude;
**true concurrent sessions require one live `claude` per session**, which a
supervisor (claude-pipe) arranges by spawning multiple channels-kit agents — not
by multiplexing chat_ids inside one.

**Routing safety.** On the outbound side, a `say`/`think`/`finish` carries the
`chat_id` Claude *echoes* — and an LLM can mis-tag it (transposition,
hallucination, or prompt-injection via task content). To avoid one session's
content leaking into another's turn, the facade **fails closed**: when exactly
one turn is open (the documented 1:1 model) it routes every tool call to that
turn and **ignores the supplied `chat_id` entirely**; only when more than one
turn is open does it trust the `chat_id`, and a tool call naming no open turn is
**dropped and logged**, never misrouted. Cross-session isolation under genuine
multiplexing therefore still depends on model obedience — which is why the
supported model is one live Claude per session.

## Why some doors are closed (not laziness)

- **No streaming *contract*.** A tool result is one atomic `CallToolResult`.
  "Streaming" here is repeated tool *calls* (a server convention), not a protocol
  feature. channels-kit uses it, honestly.
- **No server→client questions to Claude.** The MCP SDK supports
  `createMessage`/`elicitInput`/`listRoots`, but Claude-as-channel-host declares
  none of `sampling`/`elicitation`/`roots`, so those throw at the capability gate.
  Permission relay therefore uses the proprietary notification pair, **not**
  elicitation.
- **Research preview.** The push path's reliability has shifted across versions
  (e.g. a 2026-04 regression, since resolved). Pin to the running version; treat
  the wire contract as mutable.

## Net

**Full ACP** for `acp-stdio` agents (claude-pipe relays them byte-faithfully). A
**deliberately bounded but honest subset** for the subscription-Claude channels
agent: prompt → streamed message/thought chunks → `end_turn`, **plus real
permission relay** — enough for fire-a-prompt / steer / approve-a-tool / get-a-
result orchestration, but not for live tool/plan/usage observability, mode
control, mid-turn cancel, history reattach, or client-mediated fs/terminal
callbacks.
