#!/usr/bin/env node
// Minimal Claude Code Channels probe — TWO-WAY, plain Node (no Bun).
// Contract per https://code.claude.com/docs/en/channels-reference
//   - declares experimental['claude/channel'] => Claude registers a listener
//   - declares tools => Claude can discover the `reply` tool (two-way)
//   - pushes tasks via notification 'notifications/claude/channel' { content, meta }
//   - Claude calls the `reply` tool to send results back
// Claude Code spawns THIS file over stdio. The HTTP listener (localhost:8788)
// is how YOU push a task in (POST /) and watch what comes back (GET /events, SSE).
// Reference example (see README.md) — NOT the production claude-channels recipe
// (impl-plan item #13). Verified end-to-end on the subscription 2026-06-24.

import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import { ListToolsRequestSchema, CallToolRequestSchema } from '@modelcontextprotocol/sdk/types.js'
import { createServer } from 'node:http'

const PORT = 8788
const HOST = '127.0.0.1' // localhost only — nothing off this machine can POST

// --- outbound: broadcast to any `curl -N localhost:8788/events` listeners ----
const listeners = new Set()
function send(text) {
  const chunk = text.split('\n').map(l => `data: ${l}\n`).join('') + '\n'
  for (const emit of listeners) emit(chunk)
}

const mcp = new Server(
  { name: 'probe', version: '0.0.1' },
  {
    capabilities: {
      experimental: { 'claude/channel': {} }, // <- makes it a channel
      tools: {},                               // <- enables the reply tool (two-way)
    },
    instructions:
      'Messages arrive as <channel source="probe" chat_id="...">. ' +
      'They are tasks for you to act on. When you have a result, call the ' +
      '`reply` tool, passing the chat_id from the tag and your answer as text.',
  },
)

// Claude queries this at startup to discover the tool.
mcp.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [{
    name: 'reply',
    description: 'Send a message/result back over this channel',
    inputSchema: {
      type: 'object',
      properties: {
        chat_id: { type: 'string', description: 'The conversation to reply in' },
        text: { type: 'string', description: 'The message/result to send' },
      },
      required: ['chat_id', 'text'],
    },
  }],
}))

// Claude calls this when it invokes the reply tool.
mcp.setRequestHandler(CallToolRequestSchema, async req => {
  if (req.params.name === 'reply') {
    const { chat_id, text } = req.params.arguments
    send(`Reply to ${chat_id}: ${text}`)
    return { content: [{ type: 'text', text: 'sent' }] }
  }
  throw new Error(`unknown tool: ${req.params.name}`)
})

await mcp.connect(new StdioServerTransport())

// --- HTTP: GET /events streams outbound; POST / pushes a task to Claude ------
let nextId = 1
createServer((req, res) => {
  const url = new URL(req.url, `http://${HOST}:${PORT}`)

  if (req.method === 'GET' && url.pathname === '/events') {
    res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache', Connection: 'keep-alive' })
    res.write(': connected\n\n')
    const emit = chunk => res.write(chunk)
    listeners.add(emit)
    req.on('close', () => listeners.delete(emit))
    return
  }

  // anything else: read the body and forward it to Claude as a channel event
  let body = ''
  req.on('data', c => (body += c))
  req.on('end', async () => {
    const chat_id = String(nextId++)
    await mcp.notification({
      method: 'notifications/claude/channel',
      params: { content: body, meta: { chat_id, path: url.pathname, method: req.method } },
    })
    res.writeHead(200, { 'Content-Type': 'text/plain' })
    res.end(`ok (chat_id=${chat_id})\n`)
  })
}).listen(PORT, HOST, () => {
  // stderr only — stdout is reserved for the MCP/stdio protocol
  process.stderr.write(`[probe] channel server up; HTTP on http://${HOST}:${PORT} (POST / to push, GET /events to watch)\n`)
})
