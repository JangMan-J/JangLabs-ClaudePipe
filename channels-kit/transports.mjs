// transports.mjs — the pluggable seam between the two channels-kit processes.
//
// Topology (inherited from the verified bridge): the CHANNEL SERVER runs as
// Claude's MCP/stdio child; the ACP FACADE runs as a separate process (claude-pipe's
// relay child, or a standalone host). They exchange four message kinds:
//   facade → server:  { kind:'push', chat_id, content, meta }      (a task)
//                      { kind:'verdict', request_id, behavior }     (permission answer)
//   server → facade:  { kind:'tool_call', chat_id, tool, args }     (say/think/finish)
//                      { kind:'permission_request', request_id, tool_name, description, input_preview }
//
// A "bus" is the local handle each side uses; a "transport" wires two buses across
// a boundary. Two transports ship:
//   - 'inproc'      : both sides in one process (tests / embedded) — a shared emitter.
//   - 'unix-socket' : the channel server connects OUT to a facade-bound socket
//                     (mirrors the bridge's internal socket). Newline-delimited JSON.
// The HTTP standalone surface lives in cli.mjs (it drives a facade bus directly).

import net from 'node:net'
import { EventEmitter } from 'node:events'

/**
 * A bus is the per-side API the channel-server / facade code calls. It is
 * symmetric in shape but each side uses a different subset:
 *   server side uses: onPush, onPermissionVerdict, emitToolCall, emitPermissionRequest
 *   facade side uses: push, sendVerdict, onToolCall, onPermissionRequest
 */
function makeBus(send, emitter) {
  return {
    // server-side inbound (from facade)
    onPush: (h) => emitter.on('push', h),
    onPermissionVerdict: (h) => emitter.on('verdict', h),
    // server-side outbound (to facade)
    emitToolCall: (m) => send({ kind: 'tool_call', ...m }),
    emitPermissionRequest: (m) => send({ kind: 'permission_request', ...m }),
    // facade-side outbound (to server)
    push: (m) => send({ kind: 'push', ...m }),
    sendVerdict: (m) => send({ kind: 'verdict', ...m }),
    // facade-side inbound (from server)
    onToolCall: (h) => emitter.on('tool_call', h),
    onPermissionRequest: (h) => emitter.on('permission_request', h),
    _emitter: emitter,
  }
}

/** Dispatch an incoming message object onto the local emitter by its `kind`. */
function dispatch(emitter, msg) {
  if (!msg || typeof msg.kind !== 'string') return
  emitter.emit(msg.kind, msg)
}

// ---------------------------------------------------------------------------
// inproc: one process, both buses share a pair of crossed emitters.
// ---------------------------------------------------------------------------
/** Returns { serverBus, facadeBus } wired to each other in-process. */
export function inprocPair() {
  const toServer = new EventEmitter()
  const toFacade = new EventEmitter()
  const serverBus = makeBus((m) => dispatch(toFacade, m), toServer)
  const facadeBus = makeBus((m) => dispatch(toServer, m), toFacade)
  return { serverBus, facadeBus }
}

// ---------------------------------------------------------------------------
// unix-socket: facade BINDS a listener; the channel server CONNECTS to it.
// (The channel server is Claude's child and is spawned later, so it dials in.)
// ---------------------------------------------------------------------------

/**
 * Facade side: bind a unix socket and return a bus once a server connects.
 * @returns {{ bus: object, waitConnected: Promise<void>, close: () => void }}
 */
export function facadeUnixServer(sockPath) {
  const emitter = new EventEmitter()
  let conn = null
  const outbox = []
  const flush = () => {
    if (!conn) return
    while (outbox.length) conn.write(JSON.stringify(outbox.shift()) + '\n')
  }
  const send = (m) => {
    outbox.push(m)
    flush()
  }
  let resolveConn
  const waitConnected = new Promise((r) => (resolveConn = r))
  const srv = net.createServer((c) => {
    conn = c
    let buf = ''
    c.on('data', (d) => {
      buf += d.toString('utf8')
      let i
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i)
        buf = buf.slice(i + 1)
        if (!line.trim()) continue
        try {
          dispatch(emitter, JSON.parse(line))
        } catch {}
      }
    })
    c.on('close', () => {
      conn = null
    })
    flush()
    resolveConn()
  })
  srv.listen(sockPath)
  return { bus: makeBus(send, emitter), waitConnected, close: () => srv.close() }
}

/**
 * Server side: connect to the facade's unix socket and return a bus.
 * @returns {Promise<{ bus: object, close: () => void }>}
 */
export async function serverUnixClient(sockPath) {
  const emitter = new EventEmitter()
  const conn = net.createConnection(sockPath)
  await new Promise((res, rej) => {
    conn.once('connect', res)
    conn.once('error', rej)
  })
  let buf = ''
  conn.on('data', (d) => {
    buf += d.toString('utf8')
    let i
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i)
      buf = buf.slice(i + 1)
      if (!line.trim()) continue
      try {
        dispatch(emitter, JSON.parse(line))
      } catch {}
    }
  })
  const send = (m) => conn.write(JSON.stringify(m) + '\n')
  return { bus: makeBus(send, emitter), close: () => conn.destroy() }
}
