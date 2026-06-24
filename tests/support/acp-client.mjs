#!/usr/bin/env node
// Minimal stock ACP client driver for verification. Connects a Unix socket
// (the path `claude-pipe attach` printed), speaks newline-delimited JSON-RPC,
// and runs a scripted sequence. It is deliberately a *generic* ACP client — it
// has no idea the agent is pooled/warm/shared (spec Invariant 7 / §12.1).
//
// Usage: acp-client.mjs <socketPath> <command> [args...]
//   init                         -> initialize; print the result
//   prompt <sessionId> <text>    -> session/prompt; collect chunks; print result
//   newsession                   -> session/new; print the minted sessionId
//   capture <sessionId> <text>   -> like prompt but prints exact concatenated
//                                   chunk bytes length + sha for byte-fidelity
//   callback <sessionId>         -> drive a CALLBACK prompt, answering the
//                                   server-initiated fs/read_text_file request
//   raw                          -> read stdin lines, write them to the socket,
//                                   print every received line (manual scripting)

import net from 'node:net'
import crypto from 'node:crypto'

const [socketPath, command, ...rest] = process.argv.slice(2)
if (!socketPath || !command) {
  console.error('usage: acp-client.mjs <socketPath> <command> [args...]')
  process.exit(2)
}

const sock = net.createConnection(socketPath)
let buf = ''
const waiters = [] // {match, resolve}
const onLineHandlers = []

sock.on('data', (d) => {
  buf += d.toString('utf8')
  let idx
  while ((idx = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, idx)
    buf = buf.slice(idx + 1)
    if (!line.trim()) continue
    let msg
    try {
      msg = JSON.parse(line)
    } catch {
      continue
    }
    for (const h of onLineHandlers) h(msg, line)
    for (let i = waiters.length - 1; i >= 0; i--) {
      if (waiters[i].match(msg)) {
        const [w] = waiters.splice(i, 1)
        w.resolve(msg)
      }
    }
  }
})
sock.on('error', (e) => {
  console.error(`socket error: ${e.message}`)
  process.exit(1)
})

let nextId = 1
const send = (obj) => sock.write(JSON.stringify(obj) + '\n')
const sendReq = (method, params) => {
  const id = nextId++
  send({ jsonrpc: '2.0', id, method, params })
  return id
}
const waitFor = (match) => new Promise((resolve) => waiters.push({ match, resolve }))
const waitResult = (id) => waitFor((m) => m.id === id && (m.result !== undefined || m.error !== undefined))

async function connectReady() {
  await new Promise((res) => sock.once('connect', res))
}

async function main() {
  await connectReady()
  switch (command) {
    case 'init': {
      const id = sendReq('initialize', { protocolVersion: 1, clientCapabilities: {} })
      const r = await waitResult(id)
      console.log(JSON.stringify(r.result ?? r.error))
      break
    }
    case 'newsession': {
      const id = sendReq('session/new', { cwd: '/tmp', mcpServers: [] })
      const r = await waitResult(id)
      console.log(r.result?.sessionId ?? '')
      break
    }
    case 'prompt': {
      const [sessionId, ...textParts] = rest
      const text = textParts.join(' ')
      const chunks = []
      onLineHandlers.push((m) => {
        if (m.method === 'session/update' && m.params?.sessionId === sessionId) {
          const t = m.params.update?.content?.text
          if (t != null) chunks.push(t)
        }
      })
      const id = sendReq('session/prompt', { sessionId, prompt: [{ type: 'text', text }] })
      const r = await waitResult(id)
      console.log(JSON.stringify({ stopReason: r.result?.stopReason, chunks }))
      break
    }
    case 'capture': {
      const [sessionId, ...textParts] = rest
      const text = textParts.join(' ')
      let captured = ''
      onLineHandlers.push((m) => {
        if (m.method === 'session/update' && m.params?.sessionId === sessionId) {
          const t = m.params.update?.content?.text
          if (t != null) captured += t
        }
      })
      const id = sendReq('session/prompt', { sessionId, prompt: [{ type: 'text', text }] })
      const r = await waitResult(id)
      const sha = crypto.createHash('sha256').update(captured, 'utf8').digest('hex')
      console.log(JSON.stringify({ stopReason: r.result?.stopReason, len: Buffer.byteLength(captured, 'utf8'), sha }))
      break
    }
    case 'callback': {
      const [sessionId] = rest
      // Answer the agent's server-initiated fs/read_text_file with a fixed body.
      onLineHandlers.push((m) => {
        if (m.method === 'fs/read_text_file' && m.id !== undefined) {
          send({ jsonrpc: '2.0', id: m.id, result: { content: 'MOCKFILE' } })
        }
      })
      const chunks = []
      onLineHandlers.push((m) => {
        if (m.method === 'session/update' && m.params?.sessionId === sessionId) {
          const t = m.params.update?.content?.text
          if (t != null) chunks.push(t)
        }
      })
      const id = sendReq('session/prompt', { sessionId, prompt: [{ type: 'text', text: 'CALLBACK' }] })
      const r = await waitResult(id)
      console.log(JSON.stringify({ stopReason: r.result?.stopReason, chunks }))
      break
    }
    default:
      console.error(`unknown command: ${command}`)
      process.exit(2)
  }
  sock.end()
  process.exit(0)
}

main().catch((e) => {
  console.error(`client failed: ${e.stack || e}`)
  process.exit(1)
})
