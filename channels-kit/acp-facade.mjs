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
//     client and maps the client's outcome back to the channel verdict (true ACP
//     parity — not a private callback).
//
// Routing safety (audit H3): tool_calls are demultiplexed by the chat_id Claude
// echoes — but an LLM can mis-tag it. The documented model is 1 ACP session ⇄ 1
// live Claude, so when exactly ONE turn is open we route every tool_call to it and
// IGNORE the (possibly wrong) chat_id; only when >1 turn is open do we trust the
// chat_id, and a tool_call naming no open turn is dropped+logged. This fails closed
// against cross-session content leakage.

import { acpUpdate, acpResult, acpError, extractPromptText, STOP_REASONS } from './protocol.mjs'

export function createAcpFacade({ bus, write, permissionPolicy = { mode: 'allow' }, turnTimeoutMs = 180000 }) {
  const dbg = (s) => process.env.CHANNELS_KIT_DEBUG && process.stderr.write(`[acp-facade] ${s}\n`)
  const send = (obj) => write(JSON.stringify(obj) + '\n')

  let nextSession = 0
  let nextRpcId = 1_000_000 // for facade-originated requests (request_permission)
  const sessions = new Set() // sessionId (== chat_id)
  // chat_id -> { id, finished, timer, cancelRequested }
  const turns = new Map()
  // facade-originated request id -> resolver (for session/request_permission)
  const pendingClientRequests = new Map()

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
      // The active session (if exactly one open turn) carries the request; if no
      // turn is open we still ask on a synthetic session so the host can decide.
      const sid = turns.size >= 1 ? [...turns.values()][0].sessionId : 'cppipe'
      try {
        const outcome = await requestPermissionFromClient(sid, reqp)
        return outcome === 'deny' ? 'deny' : 'allow'
      } catch {
        // Fall back to the JS callback form if the client didn't answer.
        if (typeof permissionPolicy.onRequest === 'function') {
          try {
            const v = await permissionPolicy.onRequest({ ...reqp })
            return v === 'deny' ? 'deny' : 'allow'
          } catch {
            return 'allow'
          }
        }
        return 'allow'
      }
    }
    return 'allow' // default: unattended auto-approve (gate senders! — PARITY.md)
  }

  // Emit a real ACP session/request_permission request and await the client's
  // outcome.selected.optionId, mapped to allow/deny. Bounded so a silent client
  // can't wedge the channel verdict forever.
  function requestPermissionFromClient(sessionId, reqp) {
    const id = nextRpcId++
    const allowOpt = 'allow_once'
    const rejectOpt = 'reject_once'
    send({
      jsonrpc: '2.0',
      id,
      method: 'session/request_permission',
      params: {
        sessionId,
        toolCall: { toolCallId: reqp.request_id, title: reqp.tool_name, rawInput: reqp.input_preview },
        options: [
          { optionId: allowOpt, name: 'Allow', kind: 'allow_once' },
          { optionId: rejectOpt, name: 'Reject', kind: 'reject_once' },
        ],
      },
    })
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        pendingClientRequests.delete(id)
        reject(new Error('client did not answer session/request_permission'))
      }, 30000)
      pendingClientRequests.set(id, (result) => {
        clearTimeout(timer)
        const optionId = result?.outcome?.optionId ?? result?.outcome?.outcome
        resolve(optionId === rejectOpt || result?.outcome?.outcome === 'cancelled' ? 'deny' : 'allow')
      })
    })
  }

  function closeTurn(sessionId, stopReason) {
    const turn = turns.get(String(sessionId))
    if (!turn || turn.finished) return
    turn.finished = true
    if (turn.timer) clearTimeout(turn.timer)
    turns.delete(String(sessionId))
    send(acpResult(turn.id, { stopReason }))
    dbg(`turn ${sessionId} closed: ${stopReason}`)
  }

  /** Force-close every open turn (called on Claude death, audit B2). */
  function failAllTurns(stopReason = STOP_REASONS.CANCELLED) {
    for (const sid of [...turns.keys()]) closeTurn(sid, stopReason)
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
      const resolver = pendingClientRequests.get(id)
      if (resolver) {
        pendingClientRequests.delete(id)
        resolver(result)
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

  return { handleLine, failAllTurns }
}
