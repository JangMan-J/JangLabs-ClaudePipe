#!/usr/bin/env node
// delegate-live.mjs — live end-to-end check of the DELEGATE permission path through
// the claude-channels recipe (the item this branch wires). It connects as a stock ACP
// client to a data socket that is leasing a `claude-channels` agent which the recipe
// spawned with CHANNELS_KIT_PERMISSION=delegate (→ bridge auto-sets
// --permission-mode default). It then prompts Claude to run a Bash command, which
// makes Claude Code emit a tool-approval prompt; the channel relays it; the facade
// emits a REAL ACP session/request_permission to THIS client; we answer allow_once;
// and we assert the request arrived (and the turn completes).
//
// Usage: delegate-live.mjs <socketPath> <answer:allow|reject>
// Prints one JSON line: {sawPermissionRequest, answered, stopReason, toolTitle}
// Exit 0 iff a real ACP session/request_permission was received and answered.

import net from 'node:net'

const [socketPath, answer = 'allow'] = process.argv.slice(2)
if (!socketPath) {
  console.error('usage: delegate-live.mjs <socketPath> [allow|reject]')
  process.exit(2)
}

const sock = net.createConnection(socketPath)
let buf = ''
let nextId = 1
const waiters = []
const onFrame = []
sock.on('data', (d) => {
  buf += d.toString('utf8')
  let i
  while ((i = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, i)
    buf = buf.slice(i + 1)
    if (!line.trim()) continue
    let m
    try { m = JSON.parse(line) } catch { continue }
    for (const h of onFrame) h(m)
    for (let k = waiters.length - 1; k >= 0; k--) {
      if (waiters[k].match(m)) waiters.splice(k, 1)[0].resolve(m)
    }
  }
})
sock.on('error', (e) => {
  console.log(JSON.stringify({ error: `socket: ${e.message}` }))
  process.exit(1)
})

const send = (o) => sock.write(JSON.stringify(o) + '\n')
const req = (method, params) => {
  const id = nextId++
  send({ jsonrpc: '2.0', id, method, params })
  return id
}
const waitResult = (id) =>
  new Promise((resolve) => waiters.push({ match: (m) => m.id === id && (m.result !== undefined || m.error !== undefined), resolve }))

let sawPermissionRequest = false
let answered = false
let toolTitle = null

// Answer any incoming ACP session/request_permission (server-initiated request).
onFrame.push((m) => {
  if (m.method === 'session/request_permission' && m.id !== undefined) {
    sawPermissionRequest = true
    toolTitle = m.params?.toolCall?.title ?? null
    const optionId = answer === 'reject' ? 'reject_once' : 'allow_once'
    send({ jsonrpc: '2.0', id: m.id, result: { outcome: { outcome: 'selected', optionId } } })
    answered = true
  }
})

async function main() {
  await new Promise((res, rej) => {
    sock.once('connect', res)
    sock.once('error', rej)
  })
  await waitResult(req('initialize', { protocolVersion: 1, clientCapabilities: {} }))
  const sid = (await waitResult(req('session/new', { cwd: '/tmp', mcpServers: [] }))).result?.sessionId

  // Prompt Claude to actually RUN a shell command so a tool-approval fires.
  const promptId = req('session/prompt', {
    sessionId: sid,
    prompt: [{ type: 'text', text: 'Run the shell command `echo channels-delegate-probe` using the Bash tool. Do it now.' }],
  })
  const done = await Promise.race([
    waitResult(promptId),
    new Promise((res) => setTimeout(() => res({ result: { stopReason: 'TIMEOUT' } }), 110000)),
  ])

  console.log(
    JSON.stringify({
      sawPermissionRequest,
      answered,
      toolTitle,
      stopReason: done.result?.stopReason,
    })
  )
  try { sock.destroy() } catch {}
  process.exit(sawPermissionRequest && answered ? 0 : 1)
}

main().catch((e) => {
  console.log(JSON.stringify({ error: `${e.message}` }))
  process.exit(1)
})
