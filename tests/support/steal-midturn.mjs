#!/usr/bin/env node
// steal-midturn.mjs — deterministic coordinator for §12 check 5b (handoff safety
// MID-TURN: a steal MUST wait for the open turn's stopReason; §9).
//
// Why this exists (replaces the old shell-timed version): the previous check used a
// fixed `sleep 0.4` to *hope* the SLOW turn was open before timing the steal, then
// asserted an absolute wall-clock floor (`MS >= 1800`). Both are contention-fragile:
// under load (e.g. leftover `claude --channels` processes from check 7b competing for
// CPU) the backgrounded prompt might not be in flight within 0.4s — so the steal sees
// no open turn, steals immediately, and the check spuriously FAILS or stalls. The
// handoff flagged exactly this 5b flake "under resource contention".
//
// This coordinator removes the guesswork. It drives BOTH clients from one process with
// ONE monotonic clock and measures the gap between two OBSERVED events (not sleeps):
//
//   1. Client-1 opens a SLOW:<ms> turn and we WAIT for its first `working` chunk —
//      proof the turn is genuinely open and mid-stream (the mock emits this chunk
//      BEFORE its sleep). Record t_first_chunk. No fixed sleep; we gate on the
//      observed turn-open.
//   2. Only THEN does Client-2 attach (the steal). Per §9 the relay blocks the steal in
//      wait_turn_boundary until the SLOW turn's stopReason; at that boundary it tears
//      down Client-1's directions, which CLOSES Client-1's socket. So Client-1 sees an
//      EOF/'close' exactly when the steal succeeds. Record t_stolen at that close.
//   3. The §9 guarantee is then a CAUSAL duration between two observed events on one
//      clock: t_stolen - t_first_chunk must be ≈ the full SLOW window (the steal could
//      not complete until the boundary ~slowMs later). We require >= 70% of slowMs to
//      leave margin for the ~immediate first chunk and scheduling, while still being
//      far above the near-zero gap an UNSAFE immediate steal would produce.
//
// Why measure Client-1's CLOSE and not its stopReason: a stolen client never receives
// the rest of its turn — the relay reassigns the lease, so the SLOW turn's stopReason
// goes to whoever holds the lease at the boundary, NOT to the stolen Client-1. The
// socket close is the correct, relay-emitted signal that the steal reached the boundary.
//
// An UNSAFE immediate steal would close Client-1 right after attaching
// (t_stolen - t_first_chunk ≈ 0) — the exact bug this guards against. Uniform
// contention shifts both observed timestamps together and never collapses the gap.
//
// Usage: steal-midturn.mjs <socket1> <sessionId> <slowMs> <attachCmd...>
//   <socket1>      data socket already leased by client-1 (from `attach`)
//   <sessionId>    a session minted on that socket
//   <slowMs>       SLOW turn duration (e.g. 2500)
//   <attachCmd...> argv to run that prints client-2's fresh data socket on stdout
//                  (e.g. `<BIN> attach mock`) — this is the STEAL.
// Prints one JSON line: {ok, t_slow_stop, t_steal_done, steal_waited, detail}
// Exit 0 iff the steal safely waited for the boundary.

import net from 'node:net'
import { spawn } from 'node:child_process'

const [sock1, sessionId, slowMsRaw, ...attachCmd] = process.argv.slice(2)
if (!sock1 || !sessionId || !slowMsRaw || attachCmd.length === 0) {
  console.error('usage: steal-midturn.mjs <socket1> <sessionId> <slowMs> <attachCmd...>')
  process.exit(2)
}
const slowMs = parseInt(slowMsRaw, 10) || 2500

const now = () => Number(process.hrtime.bigint() / 1000000n) // ms, monotonic

// A tiny line-framed JSON-RPC client over a Unix socket. Resolves requests by id and
// lets callers observe notifications (session/update) and arbitrary frames.
function dial(path) {
  const sock = net.createConnection(path)
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
  sock.on('error', () => {}) // a steal closes the old socket; tolerate it
  const ready = new Promise((res, rej) => {
    sock.once('connect', res)
    sock.once('error', rej)
  })
  const send = (o) => sock.write(JSON.stringify(o) + '\n')
  const req = (method, params) => {
    const id = nextId++
    send({ jsonrpc: '2.0', id, method, params })
    return id
  }
  const waitResult = (id) =>
    new Promise((resolve) => waiters.push({ match: (m) => m.id === id && (m.result !== undefined || m.error !== undefined), resolve }))
  return { sock, ready, req, waitResult, onFrame, send }
}

// Run the attach command and capture the data-socket path it prints (the steal).
function runAttach(argv) {
  return new Promise((resolve, reject) => {
    const p = spawn(argv[0], argv.slice(1), { stdio: ['ignore', 'pipe', 'ignore'] })
    let out = ''
    p.stdout.on('data', (d) => (out += d.toString()))
    p.on('error', reject)
    p.on('close', (code) => {
      const sockPath = out.trim().split('\n').filter(Boolean).pop() || ''
      if (!sockPath) return reject(new Error(`attach printed no socket (exit ${code}): ${out}`))
      resolve(sockPath)
    })
  })
}

async function main() {
  const c1 = dial(sock1)
  await c1.ready

  // Observe client-1's socket CLOSE — the relay tears down the stolen client's
  // directions at the steal boundary, so this fires exactly when the steal succeeds.
  let tStolen = 0
  const stolen = new Promise((res) => {
    const mark = () => {
      if (!tStolen) {
        tStolen = now()
        res()
      }
    }
    c1.sock.once('close', mark)
    c1.sock.once('end', mark)
  })

  // Gate: resolve once client-1 sees the SLOW turn's first chunk (turn is open + live).
  let firstChunkAt = 0
  const firstChunk = new Promise((res) => {
    c1.onFrame.push((m) => {
      if (!firstChunkAt && m.method === 'session/update' && m.params?.sessionId === sessionId) {
        firstChunkAt = now()
        res()
      }
    })
  })

  // Open the SLOW turn on client-1. (Its stopReason will go to whoever holds the lease
  // at the boundary — NOT to the stolen client-1 — so we do not await it here.)
  c1.req('session/prompt', { sessionId, prompt: [{ type: 'text', text: `SLOW:${slowMs}` }] })

  // Wait until the turn is provably open (bounded, so a wedged mock can't hang us).
  await Promise.race([
    firstChunk,
    new Promise((_, rej) => setTimeout(() => rej(new Error('SLOW turn never produced a first chunk')), 8000)),
  ])

  // Now perform the steal: attach a second client, then drive a post-steal prompt to
  // confirm the new lease actually works after the boundary.
  const sock2 = await runAttach(attachCmd)
  const c2 = dial(sock2)
  await c2.ready
  const stealId = c2.req('session/prompt', { sessionId, prompt: [{ type: 'text', text: 'after-steal' }] })
  const stealResult = await c2.waitResult(stealId)
  const stealStop = stealResult.result?.stopReason

  // Ensure we have observed client-1's close (the boundary signal); bounded wait.
  await Promise.race([
    stolen,
    new Promise((res) => setTimeout(res, 3000)),
  ])

  // The causal §9 assertion: client-1 stayed leased — i.e. the steal did NOT take
  // effect — until ≈ the full SLOW window elapsed after the turn opened. A safe steal
  // waits for the boundary (~slowMs); an unsafe immediate steal closes client-1 at once.
  const waitedMs = tStolen > 0 && firstChunkAt > 0 ? tStolen - firstChunkAt : -1
  const threshold = Math.floor(slowMs * 0.7)
  const stealWaited = waitedMs >= threshold
  const ok = stealWaited && stealStop === 'end_turn'

  console.log(
    JSON.stringify({
      ok,
      first_chunk_at: firstChunkAt,
      t_stolen: tStolen,
      waited_ms: waitedMs,
      threshold_ms: threshold,
      steal_waited: stealWaited,
      steal_stop: stealStop,
      detail: ok
        ? `steal held back ${waitedMs}ms (>= ${threshold}ms) until the SLOW boundary, then leased (§9 safe)`
        : `UNSAFE: client-1 stolen after only ${waitedMs}ms (want >= ${threshold}ms); steal_stop=${stealStop}`,
    })
  )
  try { c1.sock.destroy() } catch {}
  try { c2.sock.destroy() } catch {}
  process.exit(ok ? 0 : 1)
}

main().catch((e) => {
  console.log(JSON.stringify({ ok: false, detail: `coordinator error: ${e.message}` }))
  process.exit(1)
})
