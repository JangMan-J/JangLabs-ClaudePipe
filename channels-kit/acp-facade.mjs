// acp-facade.mjs — speaks the ACP subset and maps it onto the channel bus.
//
// This is what claude-pipe's relay (or a standalone host) drives. It settles the
// §7.2 architectural question by presenting ACP on the data path. It is honest:
// it advertises only what the channel can carry (loadSession:false, text-only
// prompt caps) so a stock ACP client degrades gracefully instead of hanging.
//
// Turn model (the load-bearing mapping):
//   session/prompt(sessionId, prompt[]) →
//     bus.push({chat_id: sessionId, content: text})
//     ... collect tool_call events for this turn:
//         say(text)    → session/update agent_message_chunk   (streamed)
//         think(text)  → session/update agent_thought_chunk    (streamed)
//         finish(text) → final agent_message_chunk (if new) + respond to the prompt
//                        id with { stopReason: end_turn }
//   permission_request during the turn → policy (allow|deny|delegate). In delegate
//     mode the facade emits a REAL ACP session/request_permission request to the
//     client and maps the client's outcome (the flattened ACP v1 shape
//     outcome.optionId, with outcome.outcome as the selected/cancelled discriminator)
//     back to the channel verdict (true ACP parity — not a private callback). The
//     mapping FAILS CLOSED: allow only on an explicit allow_once; deny otherwise.
//
// Routing safety (audit H3): tool_calls are demultiplexed by the chat_id Claude
// echoes — but an LLM can mis-tag it. The documented model is 1 ACP session ⇄ 1
// live Claude, so when exactly ONE turn is open we route every tool_call to it and
// IGNORE the (possibly wrong) chat_id; only when >1 turn is open do we trust the
// chat_id, and a tool_call naming no open turn is dropped+logged. This fails closed
// against cross-session content leakage.

import { acpUpdate, acpResult, acpError, extractPromptText, STOP_REASONS } from './protocol.mjs'

export function createAcpFacade({ bus, write, permissionPolicy = { mode: 'allow' }, turnTimeoutMs = 180000, permissionTimeoutMs = 30000 }) {
  const dbg = (s) => process.env.CHANNELS_KIT_DEBUG && process.stderr.write(`[acp-facade] ${s}\n`)
  const send = (obj) => write(JSON.stringify(obj) + '\n')
  const nowMs = () => Number(process.hrtime.bigint() / 1000000n) // monotonic ms

  let nextSession = 0
  let nextRpcId = 1_000_000 // for facade-originated requests (request_permission)
  const sessions = new Set() // sessionId (== chat_id)
  // chat_id -> { id, finished, timer, cancelRequested }
  const turns = new Map()
  // facade-originated request id -> resolver (for session/request_permission)
  const pendingClientRequests = new Map()

  // Per-session timestamp of the most recent turn close. Used to suppress a stale
  // tool_call (esp. a duplicate `finish`) that a just-finished turn emits after its
  // close but just as the NEXT same-session turn opens — which would otherwise bleed
  // into or prematurely close turn N+1 (review minor: resolveTurn(turns.size===1)
  // ignores chat_id, so it can't tell a stale call apart by id). Bounded grace; a
  // real turn takes seconds, so this never drops legitimate in-turn traffic.
  const recentlyClosedAt = new Map() // sessionId -> ms
  const STALE_FINISH_GRACE_MS = 250

  // --- Resolve which open turn a tool_call belongs to (audit H3, fail-closed) ---
  function resolveTurn(chat_id) {
    if (turns.size === 1) {
      // Single-session model: route to the one open turn, ignore a mis-tagged id.
      const [only] = turns.values()
      return only
    }
    // Multiplexed: trust the chat_id, but a miss is dropped (not misrouted).
    return turns.get(String(chat_id)) || null
  }

  // --- bus inbound: tool calls from Claude (via the channel server) ---
  bus.onToolCall(({ chat_id, tool, args }) => {
    const turn = resolveTurn(chat_id)
    if (!turn) {
      dbg(`tool_call ${tool} for chat_id=${chat_id} matches no open turn — dropping (fail-closed)`)
      return
    }
    const sid = turn.sessionId
    // Stale-call guard: if this session closed a turn within the grace window and the
    // currently-open turn only just started, a `finish` here is almost certainly a
    // duplicate from the finished turn — honoring it would close turn N+1 early.
    // Drop it (the open turn's OWN finish, arriving later, closes it correctly). The
    // same window suppresses a trailing `say` leaking the prior turn's content.
    const closedAt = recentlyClosedAt.get(sid)
    const inGrace = closedAt != null && nowMs() - closedAt < STALE_FINISH_GRACE_MS
    if (inGrace && (tool === 'finish' || tool === 'say' || tool === 'think')) {
      dbg(`stale ${tool} for ${sid} within ${STALE_FINISH_GRACE_MS}ms of a close — dropping (anti-bleed)`)
      return
    }
    const text = String(args?.text ?? '')
    if (tool === 'say') {
      send(acpUpdate(sid, 'agent_message_chunk', text))
    } else if (tool === 'think') {
      send(acpUpdate(sid, 'agent_thought_chunk', text))
    } else if (tool === 'finish') {
      if (text) send(acpUpdate(sid, 'agent_message_chunk', text))
      // If the client asked to cancel this turn, honor the spec: report cancelled.
      closeTurn(sid, turn.cancelRequested ? STOP_REASONS.CANCELLED : STOP_REASONS.END_TURN)
    }
  })

  // --- bus inbound: permission requests from Claude Code (the §7.2 relay) ---
  bus.onPermissionRequest(async (reqp) => {
    const behavior = await decidePermission(reqp)
    bus.sendVerdict({ request_id: reqp.request_id, behavior })
    dbg(`permission ${reqp.request_id} (${reqp.tool_name}) -> ${behavior}`)
  })

  async function decidePermission(reqp) {
    if (permissionPolicy.mode === 'deny') return 'deny'
    if (permissionPolicy.mode === 'delegate') {
      // Prefer a REAL ACP session/request_permission to the client (true parity).
      // Attribute it to the one open turn's real session if there is exactly one;
      // if NO turn is open we cannot honestly name a session the client minted, so
      // we do NOT fabricate a synthetic sessionId on the ACP wire (a strict client
      // would reject an unknown session) — we fall through to the JS callback / the
      // fail-closed default instead (review minor: no invented 'cppipe' session).
      const sid = turns.size >= 1 ? [...turns.values()][0].sessionId : null
      if (sid) {
        try {
          const outcome = await requestPermissionFromClient(sid, reqp)
          // requestPermissionFromClient already fails CLOSED on timeout/error/
          // unknown option (returns 'deny'); 'allow' is returned ONLY on an explicit
          // allow. Pass that verdict straight through.
          return outcome === 'allow' ? 'allow' : 'deny'
        } catch {
          // The client could not be asked (e.g. write failed). Fall back below.
        }
      }
      // No attributable session, or the ACP ask could not be issued: defer to the
      // host's JS callback if it supplied one, else FAIL CLOSED. Delegate's contract
      // is "the client decides"; when the client cannot decide, deny is the only
      // safe default on a Bash/Write/Edit approval path (review major: was 'allow').
      if (typeof permissionPolicy.onRequest === 'function') {
        try {
          const v = await permissionPolicy.onRequest({ ...reqp })
          return v === 'allow' ? 'allow' : 'deny'
        } catch {
          return 'deny'
        }
      }
      return 'deny'
    }
    return 'allow' // default: unattended auto-approve (gate senders! — PARITY.md)
  }

  // Emit a real ACP session/request_permission request and await the client's
  // RequestPermissionResponse, mapping it to a fail-CLOSED verdict ('allow' | 'deny').
  //
  // ACP v1 wire shape of the response outcome is flattened with a discriminator:
  //   { outcome: { outcome: 'selected', optionId: 'allow_once' } }  (a choice)
  //   { outcome: { outcome: 'cancelled' } }                          (dismissed)
  // There is no nested `outcome.selected` object on the wire.
  //
  // FAIL CLOSED (review majors): the verdict is 'allow' ONLY on an explicit
  // allow_once selection. EVERYTHING else — reject, cancelled, a JSON-RPC error
  // response, an unknown/garbage optionId, or a 30s no-answer timeout — denies. A
  // permission decision must be deny-unless-explicitly-allowed; this is a
  // Bash/Write/Edit approval path. Bounded by a timer so a silent client cannot wedge
  // the channel verdict forever; this Promise resolves 'deny' (never rejects), so the
  // caller always gets a concrete fail-closed verdict.
  function requestPermissionFromClient(sessionId, reqp) {
    const id = nextRpcId++
    const allowOpt = 'allow_once'
    const rejectOpt = 'reject_once'
    // ACP ToolCallUpdate.rawInput is typed `object`; input_preview is a string, so
    // carry it as an object the schema accepts (review minor). Parse it when it is
    // valid JSON so a client sees the structured input; else wrap the raw preview.
    let rawInput
    try {
      const parsed = JSON.parse(reqp.input_preview ?? '')
      rawInput = parsed && typeof parsed === 'object' ? parsed : { preview: String(reqp.input_preview ?? '') }
    } catch {
      rawInput = { preview: String(reqp.input_preview ?? '') }
    }
    send({
      jsonrpc: '2.0',
      id,
      method: 'session/request_permission',
      params: {
        sessionId,
        toolCall: { toolCallId: reqp.request_id, title: reqp.tool_name, rawInput },
        options: [
          { optionId: allowOpt, name: 'Allow', kind: 'allow_once' },
          { optionId: rejectOpt, name: 'Reject', kind: 'reject_once' },
        ],
      },
    })
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        pendingClientRequests.delete(id)
        dbg(`permission request ${id} timed out — failing closed (deny)`)
        resolve('deny') // fail closed on no answer
      }, permissionTimeoutMs)
      // settle(result, error): the client's response (or, on teardown, undefined →
      // deny). Always clears the timer and removes the pending entry exactly once.
      const settle = (result, error) => {
        clearTimeout(timer)
        pendingClientRequests.delete(id)
        // A JSON-RPC error response (error present, no result) → deny.
        if (error !== undefined || result === undefined) return resolve('deny')
        const optionId = result?.outcome?.optionId
        // Allow ONLY on an explicit allow_once selection; all else denies.
        resolve(optionId === allowOpt && result?.outcome?.outcome !== 'cancelled' ? 'allow' : 'deny')
      }
      pendingClientRequests.set(id, { settle, timer })
    })
  }

  function closeTurn(sessionId, stopReason) {
    const turn = turns.get(String(sessionId))
    if (!turn || turn.finished) return
    turn.finished = true
    if (turn.timer) clearTimeout(turn.timer)
    turns.delete(String(sessionId))
    recentlyClosedAt.set(String(sessionId), nowMs()) // arm the anti-bleed grace window
    send(acpResult(turn.id, { stopReason }))
    dbg(`turn ${sessionId} closed: ${stopReason}`)
  }

  /** Force-close every open turn (called on Claude death, audit B2). */
  function failAllTurns(stopReason = STOP_REASONS.CANCELLED) {
    for (const sid of [...turns.keys()]) closeTurn(sid, stopReason)
  }

  /**
   * Drain all in-flight state on shutdown (review minor): settle every pending
   * session/request_permission (fail closed → 'deny', clearing its 30s timer) and
   * close every open turn. Without this, an embedded host that supplies onDown (so
   * the process does NOT exit) would leave permission timers live for up to 30s,
   * eventually firing a verdict on an already-torn-down bus. Idempotent.
   */
  function teardown(stopReason = STOP_REASONS.CANCELLED) {
    for (const { settle } of [...pendingClientRequests.values()]) {
      try {
        settle(undefined, undefined) // undefined result → deny; clears timer + entry
      } catch {}
    }
    failAllTurns(stopReason)
  }

  async function handleLine(line) {
    line = line.trim()
    if (!line) return
    let msg
    try {
      msg = JSON.parse(line)
    } catch {
      return
    }
    const { id, method, params, result } = msg

    // A RESPONSE from the client to a facade-originated request (no method, has id)?
    if (method === undefined && id !== undefined && (result !== undefined || msg.error !== undefined)) {
      const pending = pendingClientRequests.get(id)
      if (pending) {
        pending.settle(result, msg.error) // settle() clears the timer + map entry; an error response denies
      }
      return
    }

    switch (method) {
      case 'initialize':
        send(
          acpResult(id, {
            protocolVersion: 1,
            agentCapabilities: {
              loadSession: false,
              promptCapabilities: { image: false, audio: false, embeddedContext: false },
            },
            agentInfo: { name: 'channels-kit', title: 'Claude (channels)', version: '0.1.0' },
          })
        )
        break

      case 'session/new': {
        const sessionId = `chan-${++nextSession}`
        sessions.add(sessionId)
        send(acpResult(id, { sessionId }))
        break
      }

      case 'session/prompt': {
        const sessionId = params?.sessionId
        if (!sessions.has(sessionId)) sessions.add(sessionId)
        const text = extractPromptText(params)
        await startTurn(id, sessionId, text)
        break
      }

      case 'session/cancel': {
        // Best-effort: channels have no mid-turn cancel (PARITY.md). We MARK the
        // turn so that when it does close (via finish or timeout) it reports the
        // spec-mandated `cancelled` stopReason (ACP v1: cancelled MUST follow a
        // session/cancel) rather than a misleading end_turn. We also shorten the
        // safety timer so the client isn't stuck for the full default window.
        const turn = turns.get(String(params?.sessionId))
        if (turn) {
          turn.cancelRequested = true
          if (turn.timer) clearTimeout(turn.timer)
          turn.timer = setTimeout(() => closeTurn(turn.sessionId, STOP_REASONS.CANCELLED), 5000)
        }
        if (id !== undefined) send(acpResult(id, {})) // ack an id-bearing cancel (C3)
        dbg(`cancel ${params?.sessionId} (best-effort; will close as cancelled)`)
        break
      }

      // --- graceful stubs for surfaces the channel cannot carry (PARITY.md) ---
      case 'authenticate':
      case 'logout':
        send(acpResult(id, {}))
        break
      case 'session/load':
        send(acpResult(id, null))
        break
      case 'session/set_mode':
        send(acpResult(id, {}))
        break

      default:
        // Unknown id-bearing method → honest method-not-found, not a fake success (C4).
        if (id !== undefined) send(acpError(id, -32601, `method not found: ${method}`))
    }
  }

  async function startTurn(id, sessionId, text) {
    const chat_id = String(sessionId)
    if (turns.has(chat_id)) {
      // A re-prompt while a turn is open: close the old defensively. Documented in
      // PARITY.md. (A well-behaved single-lease client won't do this.)
      closeTurn(chat_id, STOP_REASONS.END_TURN)
    }
    const timer = setTimeout(() => {
      // Safety net: if Claude never calls finish, don't hang the client forever.
      // Emit the notice as a THOUGHT chunk (out-of-band), not a message chunk, so
      // it isn't mistaken for Claude's answer (audit C2), then close.
      dbg(`turn ${chat_id} hit timeout; closing with end_turn`)
      send(acpUpdate(sessionId, 'agent_thought_chunk', '[channels-kit] turn timed out awaiting completion'))
      closeTurn(chat_id, STOP_REASONS.END_TURN)
    }, turnTimeoutMs)
    turns.set(chat_id, { id, sessionId, finished: false, timer, cancelRequested: false })
    bus.push({ chat_id, content: text })
    dbg(`turn ${chat_id} started (pushed task)`)
  }

  return { handleLine, failAllTurns, teardown }
}
