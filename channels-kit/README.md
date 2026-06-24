# channels-kit

Expose a live **`claude --channels`** session as an **ACP-subset agent** — the
verified, subscription-safe way to drive Claude as an agent with **no `-p` and no
Agent SDK**. channels-kit pushes toward [Agent Client Protocol](https://agentclientprotocol.com)
parity on every surface the Claude Code Channels mechanism can carry, and
**honestly documents** what it cannot ([`PARITY.md`](./PARITY.md)).

It is the implementation behind claude-pipe's `claude-channels` recipe (spec
[§7.2](../docs/acp-transport-spec.md)), packaged as a standalone, reusable,
tested module.

> **Research preview.** Claude Code Channels is a research-preview surface; the
> contract may change. channels-kit is the blast-radius container for that churn,
> verified against **Claude Code v2.1.186**.

## What it does

- Runs a live `claude --channels` under a **PTY** purely to keep it interactive
  and alive (the PTY carries only Claude's discarded TUI — task/reply **data**
  rides the MCP channel, never the terminal).
- Speaks the **ACP subset** on stdio: `initialize` (honest capabilities),
  `session/new`, `session/prompt` → **streamed** `agent_message_chunk` /
  `agent_thought_chunk` → `stopReason`, plus graceful stubs for what the channel
  can't carry.
- **Relays tool-approval permission prompts** (Bash/Write/Edit) — Claude Code's
  `claude/channel/permission` ⇄ ACP `session/request_permission` — with a
  pluggable policy (allow / deny / delegate).
- Offers Claude a **multi-tool reply surface** (`say` / `think` / `finish`) so it
  streams partial results, not one terminal blob.

## Use

```sh
npm install   # @modelcontextprotocol/sdk + node-pty + zod

# Standalone HTTP: POST a task, watch streamed chunks + completion on SSE.
node cli.mjs serve --port 8790 --channel demo &
curl -N http://127.0.0.1:8790/events &           # watch
curl -X POST --data 'What is 17 * 23?' http://127.0.0.1:8790/   # push
#   → data: {"kind":"agent_message_chunk","sessionId":"chan-1","text":"391"}
#   → data: {"kind":"stopReason","stopReason":"end_turn"}

# ACP on stdio (what claude-pipe's recipe spawns): speaks raw ACP-subset frames.
node cli.mjs acp --channel demo
```

```js
// Embedded in a JS host:
import { createChannelAgent } from 'channels-kit'
const agent = await createChannelAgent({
  channelName: 'demo',
  permissionPolicy: { mode: 'delegate', onRequest: async (r) => r.tool_name === 'Bash' ? 'allow' : 'deny' },
  write: (line) => myAcpClient.feed(line),
  readStdin: false,
})
await agent.handleLine(JSON.stringify({ jsonrpc: '2.0', id: 1, method: 'initialize', params: {} }))
```

`ANTHROPIC_API_KEY` must be **unset** (subscription OAuth). Channels are
unavailable on Bedrock/Vertex/Foundry and may be org-gated.

## Architecture

Two processes (the channel server is **Claude's** MCP child; Claude is the host):

```
 ACP client / claude-pipe relay
   │  ACP subset (stdio)
   ▼
 ┌──────────────────────────── host process (createChannelAgent) ───────────┐
 │  acp-facade   ⇄  bus  ⇄  [unix socket]                                    │
 │  lifecycle → spawns `claude --dangerously-load-development-channels`      │
 │                            (PTY: liveness only, not data)                 │
 └──────────────────────────────────────────────────────────────────────────┘
                                 │ Claude spawns its MCP child:
                                 ▼
 ┌──────────── channel-server-entry (Claude's MCP/stdio child) ─────────────┐
 │  channel-server: claude/channel + claude/channel/permission +            │
 │                  say/think/finish tools  ⇄  bus  ⇄  [dials host socket]   │
 └──────────────────────────────────────────────────────────────────────────┘
```

- **`channel-server.mjs`** — the MCP server Claude spawns: capability declaration,
  the tool surface, permission relay.
- **`lifecycle.mjs`** — owns the `claude --channels` PTY (boot, auto-confirm,
  keep-alive).
- **`acp-facade.mjs`** — ACP-subset ⇄ channel bus mapping (the streaming turn,
  permission policy, honest stubs).
- **`transports.mjs`** — the facade↔server bus: `inproc` (tests) + `unix-socket`.
- **`protocol.mjs`** — verified channel wire constants + pure helpers.
- **`index.mjs` / `cli.mjs`** — public API + the `acp` / `serve` host surfaces.

## Test

```sh
node --test 'test/**/*.test.mjs'   # hermetic: no live Claude needed
```

The contract tests drive the facade with a simulated channel server (inproc bus)
and assert the ACP mapping, streaming, permission-verdict correlation, graceful
stubs, and the meta sanitizer. The **live** round-trip (real `claude --channels`)
is exercised by `cli.mjs serve` and by claude-pipe's `tests/verify.sh` check 7b
(`RUN_CLAUDE=1`).

## Parity

See [`PARITY.md`](./PARITY.md) for the full per-method CAN / PARTIAL / CANNOT map.
In short: **full ACP** for `acp-stdio` agents (claude-pipe relays them
byte-faithfully); a **deliberately bounded but honest subset** for the
subscription-Claude channels agent — prompt → streamed chunks → `end_turn`, plus
real permission relay — which is enough for fire-a-prompt / steer / approve-a-tool
/ get-a-result orchestration, but not for live tool/plan/usage observability, mode
control, mid-turn cancel, history reattach, or client-mediated fs/terminal
callbacks (those have no channel mechanism).
