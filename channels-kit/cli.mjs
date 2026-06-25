#!/usr/bin/env node
// cli.mjs — channels-kit command-line entry.
//
//   channels-kit acp [--channel <name>] [--cwd <dir>] [--permission allow|deny|delegate]
//     Speak the ACP subset on stdio (initialize/session.new/session.prompt → …).
//     This is what claude-pipe's claude-channels recipe spawns. Drives a live
//     `claude --channels` underneath. 'delegate' surfaces a real ACP
//     session/request_permission to the stdio peer (fails closed if unanswered).
//
//   channels-kit serve [--port 8790] [--channel <name>] [--permission allow|deny]
//     ('delegate' is rejected here — the HTTP/SSE surface has no permission responder.)
//     Standalone HTTP surface: POST a task to / (body = task text), GET /events
//     (SSE) to watch streamed chunks + completion. Useful without claude-pipe.
//
// Both bring up the same core (createChannelAgent); they differ only in the host
// surface (ACP-on-stdio vs HTTP).

import http from 'node:http'
import { createChannelAgent } from './index.mjs'

const argv = process.argv.slice(2)
const cmd = argv[0]
const flag = (name, def) => {
  const i = argv.indexOf(`--${name}`)
  return i >= 0 && argv[i + 1] ? argv[i + 1] : def
}

const channelName = flag('channel', 'cppipe')
const cwd = flag('cwd', process.cwd())
const permMode = flag('permission', 'allow')
// Validate the policy. 'delegate' surfaces a REAL ACP session/request_permission and
// needs an ACP client to answer it — the `acp` host has one (its stdio peer), but
// `serve` (HTTP/SSE) has NO permission responder, so a delegate request there would
// just block 30s and (now) fail closed. Rather than silently accept a dead option,
// reject delegate in serve mode and any unknown value outright (review minor).
const allowedPolicies = cmd === 'serve' ? ['allow', 'deny'] : ['allow', 'deny', 'delegate']
if (cmd === 'acp' || cmd === 'serve') {
  if (!allowedPolicies.includes(permMode)) {
    const why = permMode === 'delegate' ? 'serve has no ACP client to answer it' : 'unknown policy'
    process.stderr.write(`[channels-kit] --permission ${permMode} not valid for '${cmd}' (${why}); use ${allowedPolicies.join('|')}\n`)
    process.exit(2)
  }
}
const permissionPolicy = { mode: permMode }
// `--permission-mode default` makes Claude's tool use prompt, which ENGAGES the
// channel permission relay (otherwise the box default — often bypassPermissions —
// runs tools silently and nothing relays). Omit to inherit the box default.
const permissionMode = flag('permission-mode', undefined)

if (cmd === 'acp') {
  // ACP on stdio — the recipe path. createChannelAgent reads stdin → facade and
  // writes ACP frames to stdout.
  await createChannelAgent({ channelName, cwd, permissionPolicy, permissionMode, readStdin: true })
  process.stderr.write('[channels-kit] acp host up (ACP subset on stdio; research-preview)\n')
} else if (cmd === 'serve') {
  // Standalone HTTP: drive the facade directly and translate to SSE.
  const PORT = parseInt(flag('port', '8790'), 10)
  const HOST = '127.0.0.1'
  const listeners = new Set()
  const emit = (o) => {
    const line = `data: ${JSON.stringify(o)}\n\n`
    for (const f of listeners) f(line)
  }

  // A write sink that turns ACP frames into SSE events the HTTP client can read.
  const sink = (line) => {
    try {
      const m = JSON.parse(line)
      if (m.method === 'session/update') {
        emit({ kind: m.params.update.sessionUpdate, sessionId: m.params.sessionId, text: m.params.update.content?.text })
      } else if (m.result?.stopReason) {
        emit({ kind: 'stopReason', id: m.id, stopReason: m.result.stopReason })
      } else if (m.result?.sessionId) {
        emit({ kind: 'session', sessionId: m.result.sessionId })
      }
    } catch {}
  }

  const agent = await createChannelAgent({ channelName, cwd, permissionPolicy, permissionMode, write: sink, readStdin: false })

  // Drive a single standalone session.
  let sid = null
  let rpc = 1
  const init = async () => {
    await agent.handleLine(JSON.stringify({ jsonrpc: '2.0', id: rpc++, method: 'initialize', params: {} }))
    await agent.handleLine(JSON.stringify({ jsonrpc: '2.0', id: rpc++, method: 'session/new', params: {} }))
  }
  // Capture the minted sessionId from the SSE side.
  listeners.add((line) => {
    const m = line.startsWith('data: ') ? JSON.parse(line.slice(6)) : null
    if (m?.kind === 'session') sid = m.sessionId
  })
  await init()

  http
    .createServer((req, res) => {
      const url = new URL(req.url, `http://${HOST}:${PORT}`)
      if (req.method === 'GET' && url.pathname === '/events') {
        res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-cache', Connection: 'keep-alive' })
        res.write(': connected\n\n')
        const f = (c) => res.write(c)
        listeners.add(f)
        req.on('close', () => listeners.delete(f))
        return
      }
      let body = ''
      req.on('data', (c) => (body += c))
      req.on('end', async () => {
        if (!sid) {
          res.writeHead(503)
          return res.end('session not ready\n')
        }
        await agent.handleLine(
          JSON.stringify({ jsonrpc: '2.0', id: rpc++, method: 'session/prompt', params: { sessionId: sid, prompt: [{ type: 'text', text: body }] } })
        )
        res.writeHead(200)
        res.end('ok\n')
      })
    })
    .listen(PORT, HOST, () => process.stderr.write(`[channels-kit] serve up; POST / to push, GET /events to watch (http://${HOST}:${PORT})\n`))
} else {
  process.stderr.write('usage: channels-kit <acp|serve> [--channel name] [--cwd dir] [--permission allow|deny] [--port N]\n')
  process.exit(2)
}
