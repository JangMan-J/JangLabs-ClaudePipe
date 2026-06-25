# channel-probe — minimal Claude Code Channels proof-of-concept

A **reference example**, not the recipe. This is the throwaway two-way channel
server used to verify, end-to-end on a real subscription (2026-06-24), that
**Claude Code Channels can prompt interactive Claude as an agent without `-p` and
without the Agent SDK** — the strategic linchpin of the ACP-transport pivot (see
`../../acp-transport-spec.md` §7.2 + Appendix A.1). The production
`claude-channels` recipe is impl-plan item #13; **do not mistake this probe for
it.**

It is the smallest thing that exercises the `claude/channel` contract:
- declares `capabilities.experimental['claude/channel']` (registers the listener)
  + `capabilities.tools` (two-way → the `reply` tool)
- pushes a task via `notifications/claude/channel` `{ content, meta }`
- Claude returns its result by calling the `reply` tool
- stdio to Claude Code (which spawns it as a subprocess — this is why it rides the
  subscription: the native interactive app spawns the helper); a localhost HTTP
  listener (`:8788`) is how *you* push tasks in and watch replies out

## Run it

Requires Node (any of Node/Bun/Deno work; this uses plain Node ESM + `node:http`)
and Claude Code ≥ v2.1.80 (Channels floor).

```sh
cd docs/examples/channel-probe
npm install                         # pulls @modelcontextprotocol/sdk

# Terminal 1 — launch Claude with the channel (subscription, not API key):
unset ANTHROPIC_API_KEY
claude --dangerously-load-development-channels server:probe
#   → accept "Use this MCP server"
#   → look for the dim banner: "Channels (experimental) messages from
#     server:probe inject directly in this session" = enabled

# Terminal 2 — watch what Claude sends back:
curl -N localhost:8788/events

# Terminal 3 — push a task into the live session:
curl -d "What is 17 * 23? Reply with just the number." localhost:8788
#   → arrives as <channel source="probe" chat_id="1">…
#   → Claude works it, calls reply → "Reply to 1: 391" on the /events stream
```

## Caveats (carry these — same as the recipe)

1. **Research preview** — `--channels` syntax/contract may change.
2. **`--dangerously-load-development-channels`** is required for custom (non-
   allowlisted) channels; it prompts for confirmation. Only point it at servers
   you trust.
3. **Steers an already-running session** — keep a `claude` alive; not fire-and-
   forget. For unattended use you'd add the permission-relay capability and gate
   senders (see spec §7.2 / the docs `channels-reference`).

Source contract: <https://code.claude.com/docs/en/channels-reference>
