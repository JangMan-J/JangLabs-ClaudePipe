#!/usr/bin/env node
// claude-channels-bridge — the §7.2 strategic recipe's bridge binary.
//
// Presents a MINIMAL ACP subset on its stdio (toward the claude-pipe relay /
// orchestrator) while internally driving a live, subscription-authenticated
// `claude --dangerously-load-development-channels` session. This is the ONE
// verified subscription-safe way to prompt Claude as an agent without `-p` or
// the Agent SDK (spec Appendix A.1; demonstrated end-to-end 2026-06-24).
//
// ── Why a bridge (not raw ACP) ──────────────────────────────────────────────
// Claude Code Channels is NOT ACP: a channel server is an MCP server Claude
// SPAWNS as a child; tasks are pushed via `notifications/claude/channel`; Claude
// replies by calling a `reply` tool. So the data flow is Channels↔MCP, and Claude
// is the PARENT of the channel server. The bridge contains 100% of that mismatch
// + the research-preview churn (§7.2 blast-radius container) and translates it to
// the same ACP subset the acp-stdio relay already speaks — so the orchestrator's
// interface stays uniform. The bridge is a SEPARATE binary; it never touches the
// acp-stdio data-path code, so it cannot contaminate that purity (§7.2 note).
//
// ── Topology (two processes, one file) ──────────────────────────────────────
//   bridge-main  (relay's child): owns ACP-subset stdio ⇄ relay; binds an
//                internal Unix socket; spawns `claude` pointed at a temp MCP
//                config that runs THIS FILE in channel-server mode.
//   channel-srv  (claude's child; `--as-channel-server <sock>`): the MCP/stdio
//                server Claude talks to. Declares the `claude/channel` capability
//                + a `reply` tool, connects to bridge-main's socket, and ferries
//                pushed tasks → Claude and Claude's `reply` → bridge-main.
//
// ── ACP subset presented to the relay ───────────────────────────────────────
//   initialize        -> result advertising NO loadSession (channels has no
//                        history-replay; recipe-level property to document, §9).
//   session/new       -> result { sessionId } (the bridge mints chan-1, chan-2…;
//                        each ACP session maps to one channel chat_id).
//   session/prompt    -> push the prompt text as a channel task to the live
//                        Claude; stream Claude's reply as a session/update chunk;
//                        respond to the prompt id with { stopReason: "end_turn" }.
//   session/cancel    -> best-effort (channels has no first-class cancel).
//
// CAVEATS (carry, do not bury — §7.2): (1) research preview — flag/contract may
// change; this file is where that churn lands. (2) needs
// `--dangerously-load-development-channels`. (3) steers an already-running
// session — the bridge keeps `claude` alive (not fire-and-forget).

import net from 'node:net'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { spawn } from 'node:child_process'
import { createInterface } from 'node:readline'

const MODE_CHANNEL_SERVER = process.argv.includes('--as-channel-server')
const dbg = (s) => process.env.CHANNELS_BRIDGE_DEBUG && process.stderr.write(`[bridge] ${s}\n`)

if (MODE_CHANNEL_SERVER) {
  await runChannelServer()
} else {
  await runBridgeMain()
}

// ===========================================================================
// bridge-main: ACP subset ⇄ relay, owns Claude's lifecycle + the internal socket
// ===========================================================================
async function runBridgeMain() {
  // Internal Unix socket the channel-server (Claude's child) connects back to.
  const sockPath = path.join(
    os.tmpdir(),
    `cp-channels-${process.pid}-${Date.now()}.sock`
  )
  try {
    fs.unlinkSync(sockPath)
  } catch {}

  // chat_id -> { resolveReply } for in-flight prompts awaiting Claude's reply.
  const inflight = new Map()
  let channelConn = null

  const internal = net.createServer((conn) => {
    dbg('channel-server connected to internal socket')
    channelConn = conn
    let buf = ''
    conn.on('data', (d) => {
      buf += d.toString('utf8')
      let i
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i)
        buf = buf.slice(i + 1)
        if (!line.trim()) continue
        let msg
        try {
          msg = JSON.parse(line)
        } catch {
          continue
        }
        // A reply from Claude (via the channel server) for a chat_id.
        if (msg.type === 'reply') {
          const w = inflight.get(String(msg.chat_id))
          if (w) w.onReply(msg.text)
        }
      }
    })
    conn.on('close', () => {
      channelConn = null
      dbg('channel-server disconnected')
    })
  })
  internal.listen(sockPath)

  // Launch Claude with a temp MCP config that runs THIS FILE as the channel
  // server. Subscription OAuth: ANTHROPIC_API_KEY is already unset by the recipe.
  const mcpConfig = {
    mcpServers: {
      cppipe: {
        command: process.execPath, // node
        args: [path.resolve(process.argv[1]), '--as-channel-server', sockPath],
      },
    },
  }
  const cfgPath = path.join(os.tmpdir(), `cp-channels-mcp-${process.pid}.json`)
  fs.writeFileSync(cfgPath, JSON.stringify(mcpConfig))

  // Keep `claude` ALIVE for the bridge's lifetime (§7.2 caveat 3). We run it
  // headless-but-interactive via its channels surface; it idles until tasks push.
  const claude = spawn(
    'claude',
    [
      '--dangerously-load-development-channels',
      'cppipe',
      '--mcp-config',
      cfgPath,
    ],
    { stdio: ['ignore', 'pipe', 'inherit'] }
  )
  claude.stdout.on('data', (d) => dbg(`claude: ${d.toString().trim().slice(0, 120)}`))
  claude.on('exit', (code) => {
    process.stderr.write(`[bridge] claude exited (code ${code}); bridge shutting down\n`)
    process.exit(code ?? 1)
  })

  // ACP subset on our stdio toward the relay.
  let nextSession = 0
  const out = (obj) => process.stdout.write(JSON.stringify(obj) + '\n')
  const rl = createInterface({ input: process.stdin })
  rl.on('line', async (line) => {
    line = line.trim()
    if (!line) return
    let msg
    try {
      msg = JSON.parse(line)
    } catch {
      return
    }
    const { id, method, params } = msg
    switch (method) {
      case 'initialize':
        // No loadSession: channels has no history-replay (§9 recipe property).
        out({ jsonrpc: '2.0', id, result: { protocolVersion: 1, agentCapabilities: { loadSession: false } } })
        break
      case 'session/new': {
        const sessionId = `chan-${++nextSession}`
        out({ jsonrpc: '2.0', id, result: { sessionId } })
        break
      }
      case 'session/prompt': {
        const sessionId = params?.sessionId
        const text = extractText(params)
        await driveTurn({ id, sessionId, text, inflight, getConn: () => channelConn, out })
        break
      }
      case 'session/cancel':
        dbg(`cancel ${params?.sessionId} (channels has no first-class cancel)`)
        break
      default:
        if (id !== undefined) out({ jsonrpc: '2.0', id, result: {} })
    }
  })

  const cleanup = () => {
    try { claude.kill('SIGTERM') } catch {}
    try { fs.unlinkSync(sockPath) } catch {}
    try { fs.unlinkSync(cfgPath) } catch {}
  }
  process.on('SIGTERM', () => { cleanup(); process.exit(0) })
  process.on('SIGINT', () => { cleanup(); process.exit(0) })
  process.stderr.write('[bridge] claude-channels bridge up (ACP subset on stdio; research-preview)\n')
}

// Push one prompt as a channel task; stream the reply as an ACP chunk; close turn.
async function driveTurn({ id, sessionId, text, inflight, getConn, out }) {
  const chat_id = sessionId // 1 ACP session ⇄ 1 channel chat_id
  const conn = getConn()
  const chunk = (t) =>
    out({
      jsonrpc: '2.0',
      method: 'session/update',
      params: { sessionId, update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: t } } },
    })

  if (!conn) {
    chunk('[bridge] channel server not yet connected to Claude; retry shortly')
    out({ jsonrpc: '2.0', id, result: { stopReason: 'refusal' } })
    return
  }

  const replyText = await new Promise((resolve) => {
    inflight.set(String(chat_id), { onReply: (t) => { inflight.delete(String(chat_id)); resolve(t) } })
    // Tell the channel server to push this task into the live Claude session.
    conn.write(JSON.stringify({ type: 'push', chat_id, content: text }) + '\n')
  })

  chunk(replyText)
  out({ jsonrpc: '2.0', id, result: { stopReason: 'end_turn' } })
}

function extractText(params) {
  const blocks = params?.prompt
  if (Array.isArray(blocks)) return blocks.map((b) => (typeof b === 'string' ? b : b?.text ?? '')).join('')
  if (typeof params?.text === 'string') return params.text
  return ''
}

// ===========================================================================
// channel-server: Claude's MCP/stdio child. Declares claude/channel + reply tool.
// Mirrors the proven probe (docs/examples/channel-probe/server.mjs) but ferries
// over the internal Unix socket instead of HTTP.
// ===========================================================================
async function runChannelServer() {
  const sockPath = process.argv[process.argv.indexOf('--as-channel-server') + 1]
  const { Server } = await import('@modelcontextprotocol/sdk/server/index.js')
  const { StdioServerTransport } = await import('@modelcontextprotocol/sdk/server/stdio.js')
  const { ListToolsRequestSchema, CallToolRequestSchema } = await import('@modelcontextprotocol/sdk/types.js')

  // Connect back to bridge-main.
  const conn = net.createConnection(sockPath)
  await new Promise((res, rej) => {
    conn.once('connect', res)
    conn.once('error', rej)
  })

  const mcp = new Server(
    { name: 'cppipe-channel', version: '0.1.0' },
    {
      capabilities: {
        experimental: { 'claude/channel': {} }, // registers the channel listener
        tools: {}, // enables the reply tool (two-way)
      },
      instructions:
        'Messages arrive as <channel source="cppipe" chat_id="...">. Each is a task ' +
        'for you to act on. When done, call the `reply` tool with the chat_id from ' +
        'the tag and your result as text.',
    }
  )

  mcp.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: [
      {
        name: 'reply',
        description: 'Send a result back over this channel',
        inputSchema: {
          type: 'object',
          properties: {
            chat_id: { type: 'string', description: 'The conversation to reply in' },
            text: { type: 'string', description: 'The result to send' },
          },
          required: ['chat_id', 'text'],
        },
      },
    ],
  }))

  mcp.setRequestHandler(CallToolRequestSchema, async (req) => {
    if (req.params.name === 'reply') {
      const { chat_id, text } = req.params.arguments
      conn.write(JSON.stringify({ type: 'reply', chat_id, text }) + '\n')
      return { content: [{ type: 'text', text: 'sent' }] }
    }
    throw new Error(`unknown tool: ${req.params.name}`)
  })

  await mcp.connect(new StdioServerTransport())

  // Receive `push` tasks from bridge-main and inject them as channel notifications.
  let buf = ''
  conn.on('data', async (d) => {
    buf += d.toString('utf8')
    let i
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i)
      buf = buf.slice(i + 1)
      if (!line.trim()) continue
      let msg
      try {
        msg = JSON.parse(line)
      } catch {
        continue
      }
      if (msg.type === 'push') {
        await mcp.notification({
          method: 'notifications/claude/channel',
          params: { content: msg.content, meta: { chat_id: String(msg.chat_id) } },
        })
      }
    }
  })
  process.stderr.write('[bridge:channel-server] up; bridging Claude channel ⇄ internal socket\n')
}
