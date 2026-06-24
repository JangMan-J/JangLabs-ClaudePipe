#!/usr/bin/env node
// Mock ACP agent — a deterministic, scriptable stdio ACP agent for verifying the
// claude-pipe relay against spec §12. It speaks just enough real ACP wire format
// (newline-delimited JSON-RPC 2.0) to exercise the relay's two parsed fields
// (sessionId + prompt/stopReason) and the server-initiated callback path.
//
// It is NOT a real agent — it has no model. It mechanically answers the client's
// JSON-RPC so the relay sees authentic frames:
//   - initialize            -> result advertising loadSession
//   - session/new           -> result { sessionId } (mints ids sess-1, sess-2, …)
//   - session/prompt        -> streams session/update chunks for that sessionId,
//                              then responds to the prompt id with { stopReason }.
//                              Behavior is controlled by the prompt text:
//                                "BIG:<n>"   -> one chunk of n 'x' bytes (byte-fidelity)
//                                "SLOW:<ms>" -> wait ms before the stopReason (turn-open window)
//                                "CALLBACK"  -> issue a server-initiated fs/read_text_file
//                                               request mid-turn, wait for the client's
//                                               response, then stop
//                                "FLOOD:<n>" -> emit n chunks rapidly (fairness/overflow)
//                                else        -> a single echo chunk
//   - session/cancel        -> notification; we stop the session's turn
//
// stdout carries ONLY ACP frames (one JSON per line, no embedded newlines).
// stderr is for debug. This mirrors the ACP transport contract exactly.

import { createInterface } from 'node:readline'

let nextSession = 0
const out = (obj) => process.stdout.write(JSON.stringify(obj) + '\n')
const dbg = (s) => process.env.MOCK_DEBUG && process.stderr.write(`[mock] ${s}\n`)

// pending server-initiated requests we issued, awaiting the client's response
const pendingCallbacks = new Map() // id -> resolve

const rl = createInterface({ input: process.stdin })
rl.on('line', (line) => {
  line = line.trim()
  if (!line) return
  let msg
  try {
    msg = JSON.parse(line)
  } catch (e) {
    dbg(`unparseable: ${line}`)
    return
  }

  // A response to one of our server-initiated callbacks?
  if (msg.id !== undefined && msg.method === undefined && (msg.result !== undefined || msg.error !== undefined)) {
    const r = pendingCallbacks.get(msg.id)
    if (r) {
      pendingCallbacks.delete(msg.id)
      r(msg)
      return
    }
  }

  const { id, method, params } = msg
  switch (method) {
    case 'initialize':
      out({ jsonrpc: '2.0', id, result: { protocolVersion: 1, agentCapabilities: { loadSession: true } } })
      break
    case 'session/new': {
      const sessionId = `sess-${++nextSession}`
      out({ jsonrpc: '2.0', id, result: { sessionId } })
      break
    }
    case 'session/load':
      out({ jsonrpc: '2.0', id, result: {} })
      break
    case 'session/prompt':
      handlePrompt(id, params).catch((e) => dbg(`prompt error: ${e}`))
      break
    case 'session/cancel':
      // notification; nothing to ack
      dbg(`cancel ${params?.sessionId}`)
      break
    default:
      // Unknown method with an id → minimal empty result (stay byte-legal).
      if (id !== undefined) out({ jsonrpc: '2.0', id, result: {} })
  }
})

let nextServerId = 10000
async function handlePrompt(id, params) {
  const sessionId = params?.sessionId
  const text = extractText(params)
  dbg(`prompt sess=${sessionId} text=${JSON.stringify(text)}`)

  const chunk = (content) =>
    out({
      jsonrpc: '2.0',
      method: 'session/update',
      params: { sessionId, update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: content } } },
    })

  if (text.startsWith('BIG:')) {
    const n = parseInt(text.slice(4), 10) || 1000
    chunk('X'.repeat(n))
  } else if (text.startsWith('SLOW:')) {
    const ms = parseInt(text.slice(5), 10) || 1000
    chunk('working')
    await sleep(ms)
  } else if (text.startsWith('FLOOD:')) {
    const n = parseInt(text.slice(6), 10) || 100
    for (let i = 0; i < n; i++) chunk(`chunk-${i}`)
  } else if (text === 'CALLBACK') {
    // Server-initiated request mid-turn; wait for the client's response.
    const cbId = ++nextServerId
    const got = new Promise((res) => pendingCallbacks.set(cbId, res))
    out({ jsonrpc: '2.0', id: cbId, method: 'fs/read_text_file', params: { sessionId, path: '/etc/hostname' } })
    const resp = await got
    chunk(`callback-got:${JSON.stringify(resp.result ?? resp.error)}`)
  } else {
    chunk(`echo:${text}`)
  }

  out({ jsonrpc: '2.0', id, result: { stopReason: 'end_turn' } })
}

function extractText(params) {
  const blocks = params?.prompt
  if (Array.isArray(blocks)) {
    return blocks.map((b) => (typeof b === 'string' ? b : b?.text ?? '')).join('')
  }
  if (typeof params?.text === 'string') return params.text
  return ''
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
process.stderr.write('[mock] mock ACP agent up on stdio\n')
