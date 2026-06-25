#!/usr/bin/env node
// channel-server — the MCP/stdio server Claude spawns as a child (Claude is the
// MCP host/client; this is the server). It is the Channels↔channels-kit seam.
//
// Responsibilities (all grounded in the channels-reference contract, verified on
// Claude Code v2.1.186):
//   - Declare experimental['claude/channel']={} so Claude registers the listener.
//   - Declare experimental['claude/channel/permission']={} so tool-approval
//     prompts relay (Claude Code >= v2.1.81).  [§7.2 "permission prompts relay"]
//   - Declare tools:{} and expose a SERVER-NAMED tool surface (say/think/finish)
//     that Claude calls back through — streaming via repeated say/think calls,
//     closing via finish.  [beyond-floor streaming; §7.2 requires only one reply]
//   - PUSH tasks via notifications/claude/channel {content, meta} (meta keys
//     sanitized to [A-Za-z0-9_]).
//   - RELAY permission: handle inbound notifications/claude/channel/permission_request
//     and answer notifications/claude/channel/permission {request_id, behavior}.
//
// This module is TRANSPORT-AGNOSTIC about how it talks to the rest of channels-kit:
// it is driven by an injected `bus` (see transports.mjs) that delivers `push`
// commands in and carries tool-calls / permission events out. So the same server
// works whether channels-kit runs in-process, over a unix socket, or over HTTP.

import { Server } from '@modelcontextprotocol/sdk/server/index.js'
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js'
import {
  ListToolsRequestSchema,
  CallToolRequestSchema,
} from '@modelcontextprotocol/sdk/types.js'
import { z } from 'zod'
import { sanitizeMeta, PERMISSION_REQUEST, PERMISSION_VERDICT, CHANNEL_PUSH } from './protocol.mjs'

/**
 * Build and connect the channel server over stdio.
 *
 * @param {object} opts
 * @param {string} opts.name      Channel name → becomes the `source=` tag attribute.
 * @param {string} opts.instructions  Steers Claude on how to use the tool surface.
 * @param {object} opts.bus       Event bus to the rest of channels-kit. Must expose:
 *                                  - onPush(handler): handler({chat_id, content, meta})
 *                                  - onPermissionVerdict(handler): handler({request_id, behavior})
 *                                  - emitToolCall({chat_id, tool, args})
 *                                  - emitPermissionRequest({request_id, tool_name, description, input_preview})
 * @returns {Promise<{server: Server, close: () => Promise<void>}>}
 */
export async function startChannelServer({ name, instructions, bus }) {
  const dbg = (s) => process.env.CHANNELS_KIT_DEBUG && process.stderr.write(`[channel-server] ${s}\n`)

  const server = new Server(
    { name, version: '0.1.0' },
    {
      capabilities: {
        experimental: {
          'claude/channel': {}, // register the channel listener
          'claude/channel/permission': {}, // opt into permission relay (>=2.1.81)
        },
        tools: {}, // enable the tool surface (two-way)
      },
      instructions,
    }
  )

  // --- Tool surface: say (stream a chunk) / think (thought chunk) / finish (close).
  // Server-NAMED tools; Claude is steered to use them by `instructions`. All take
  // chat_id so multiple conversations can be demultiplexed on the way out.
  server.setRequestHandler(ListToolsRequestSchema, async () => ({
    tools: [
      {
        name: 'say',
        description:
          'Stream a chunk of your visible answer back to the operator. Call as many ' +
          'times as you like during the task to stream partial results. Pass chat_id.',
        inputSchema: {
          type: 'object',
          properties: {
            chat_id: { type: 'string', description: 'The conversation id from the <channel> tag.' },
            text: { type: 'string', description: 'A chunk of your answer.' },
          },
          required: ['chat_id', 'text'],
        },
      },
      {
        name: 'think',
        description:
          'Optionally share a short reasoning/progress note (a thought). Streamed ' +
          'separately from your answer. Pass chat_id.',
        inputSchema: {
          type: 'object',
          properties: {
            chat_id: { type: 'string' },
            text: { type: 'string', description: 'A brief thought or progress note.' },
          },
          required: ['chat_id', 'text'],
        },
      },
      {
        name: 'finish',
        description:
          'Signal that the task is COMPLETE. Pass chat_id and your final answer text ' +
          '(may repeat the last say, or summarize). Always call finish exactly once ' +
          'when done.',
        inputSchema: {
          type: 'object',
          properties: {
            chat_id: { type: 'string' },
            text: { type: 'string', description: 'Your final answer.' },
          },
          required: ['chat_id', 'text'],
        },
      },
    ],
  }))

  server.setRequestHandler(CallToolRequestSchema, async (req) => {
    const tool = req.params.name
    const args = req.params.arguments ?? {}
    const chat_id = String(args.chat_id ?? '')
    dbg(`tool ${tool} chat_id=${chat_id} text=${JSON.stringify(args.text)?.slice(0, 80)}`)
    if (tool === 'say' || tool === 'think' || tool === 'finish') {
      bus.emitToolCall({ chat_id, tool, args })
      return { content: [{ type: 'text', text: 'ok' }] }
    }
    throw new Error(`unknown tool: ${tool}`)
  })

  // --- Permission relay (inbound): Claude Code → server permission_request.
  // The SDK validates against a Zod schema; we register a handler keyed by the
  // proprietary method literal. request_id is 5 letters [a-km-z] (no 'l').
  const PermissionRequestSchema = z.object({
    method: z.literal(PERMISSION_REQUEST),
    params: z
      .object({
        request_id: z.string(),
        tool_name: z.string().optional(),
        description: z.string().optional(),
        input_preview: z.string().optional(),
      })
      .passthrough(),
  })
  server.setNotificationHandler(PermissionRequestSchema, async (note) => {
    const p = note.params
    dbg(`permission_request ${p.request_id} tool=${p.tool_name}`)
    bus.emitPermissionRequest({
      request_id: p.request_id,
      tool_name: p.tool_name ?? '',
      description: p.description ?? '',
      input_preview: p.input_preview ?? '',
    })
  })

  await server.connect(new StdioServerTransport())
  server.oninitialized = () => {
    // One-time visibility into what Claude actually declares over the channel
    // (expected: experimental only — no sampling/elicitation/roots; see PARITY.md).
    try {
      dbg('client capabilities: ' + JSON.stringify(server.getClientCapabilities?.() ?? 'n/a'))
    } catch {}
  }

  // --- Wire the bus → server outbound.
  // PUSH a task into the live session.
  bus.onPush(async ({ chat_id, content, meta }) => {
    const safeMeta = sanitizeMeta({ chat_id: String(chat_id), ...(meta ?? {}) })
    await server.notification({ method: CHANNEL_PUSH, params: { content: String(content), meta: safeMeta } })
    dbg(`pushed task chat_id=${chat_id}`)
  })
  // ANSWER a permission request.
  bus.onPermissionVerdict(async ({ request_id, behavior }) => {
    await server.notification({ method: PERMISSION_VERDICT, params: { request_id, behavior } })
    dbg(`permission verdict ${request_id} -> ${behavior}`)
  })

  process.stderr.write(`[channel-server] up; channel '${name}' (claude/channel + permission, say/think/finish)\n`)
  return {
    server,
    close: async () => {
      try {
        await server.close()
      } catch {}
    },
  }
}
