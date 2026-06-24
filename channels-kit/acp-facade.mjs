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
//     ... collect tool_call events for this chat_id:
//         say(text)    → session/update agent_message_chunk   (streamed; beyond-floor)
//         think(text)  → session/update agent_thought_chunk    (streamed; beyond-floor)
//         finish(text) → final agent_message_chunk (if new) + respond to the prompt
//                        id with { stopReason: end_turn }       (§7.2 reply → stopReason)
//   permission_request during the turn → onPermission(policy) → bus.sendVerdict
//
// Everything the channel cannot carry is a graceful stub (see PARITY.md): load,
// set_mode, authenticate, logout, fs/*, terminal/*, tool_call/plan/usage telemetry.

import { acpUpdate, acpResult, extractPromptText, STOP_REASONS } from './protocol.mjs'

/**
 * Create the ACP facade.
 *
 * @param {object} opts
 * @param {object} opts.bus     A facade-side bus (push/sendVerdict/onToolCall/onPermissionRequest).
 * @param {(line: string) => void} opts.write   Sink for ACP frames (one JSON per line).
 * @param {object} [opts.permissionPolicy]  How to answer relayed tool-approval prompts:
 *        { mode: 'allow' | 'deny' | 'delegate', onRequest?: async (req) => 'allow'|'deny' }
 *        'delegate' surfaces an ACP-shaped session/request_permission to the host
 *        via onRequest and uses its verdict; default is 'allow' (unattended).
 * @param {number} [opts.turnTimeoutMs]  Max wait for a finish before forcing a close.
 * @returns {{ handleLine: (line: string) => Promise<void>, ready: () => void }}
 */
export function createAcpFacade({ bus, write, permissionPolicy = { mode: 'allow' }, turnTimeoutMs = 180000 }) {
  const dbg = (s) => process.env.CHANNELS_KIT_DEBUG && process.stderr.write(`[acp-facade] ${s}\n`)
  const send = (obj) => write(JSON.stringify(obj) + '\n')

  let nextSession = 0
  const sessions = new Set() // sessionId (== chat_id)
  // chat_id -> { id (prompt rpc id), finished, timer } for the in-flight turn.
  const turns = new Map()

  // --- bus inbound: tool calls from Claude (via the channel server) ---
  bus.onToolCall(({ chat_id, tool, args }) => {
    const turn = turns.get(String(chat_id))
    if (!turn) {
      dbg(`tool_call for ${chat_id} with no active turn (late?) — ignoring`)
      return
    }
    const text = String(args?.text ?? '')
    if (tool === 'say') {
      send(acpUpdate(chat_id, 'agent_message_chunk', text))
    } else if (tool === 'think') {
      send(acpUpdate(chat_id, 'agent_thought_chunk', text))
    } else if (tool === 'finish') {
      // Emit the final text as a chunk (so the whole answer is on the wire even if
      // Claude only called finish), then close the turn.
      if (text) send(acpUpdate(chat_id, 'agent_message_chunk', text))
      closeTurn(chat_id, STOP_REASONS.END_TURN)
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
    if (permissionPolicy.mode === 'delegate' && typeof permissionPolicy.onRequest === 'function') {
      // Surface an ACP-shaped session/request_permission to the host and use its
      // verdict. The host answers 'allow' | 'deny'. (We DON'T put a JSON-RPC id on
      // the channel side — the relay is fire-and-forget by request_id, so there's
      // no orphanable id; this dovetails with spec §9 steal-safety.)
      try {
        const v = await permissionPolicy.onRequest({
          tool_name: reqp.tool_name,
          description: reqp.description,
          input_preview: reqp.input_preview,
          request_id: reqp.request_id,
        })
        return v === 'deny' ? 'deny' : 'allow'
      } catch {
        return 'allow'
      }
    }
    return 'allow' // default: unattended auto-approve (gate senders! — PARITY.md)
  }

  function closeTurn(chat_id, stopReason) {
    const turn = turns.get(String(chat_id))
    if (!turn || turn.finished) return
    turn.finished = true
    if (turn.timer) clearTimeout(turn.timer)
    turns.delete(String(chat_id))
    send(acpResult(turn.id, { stopReason }))
    dbg(`turn ${chat_id} closed: ${stopReason}`)
  }

  async function handleLine(line) {
    line = line.trim()
    if (!line) return
    let msg
    try {
      msg = JSON.parse(line)
    } catch {
      return // not a JSON-RPC frame
    }
    const { id, method, params } = msg
    switch (method) {
      case 'initialize':
        // Honest capabilities: no history replay; text-only prompt content. The
        // client thus won't send image/audio/resource blocks or call session/load.
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
        if (!sessions.has(sessionId)) {
          // Tolerate a prompt on an unknown session by registering it (the relay
          // mints session ids; we stay permissive rather than erroring).
          sessions.add(sessionId)
        }
        const text = extractPromptText(params)
        await startTurn(id, sessionId, text)
        break
      }

      case 'session/cancel':
        // Best-effort only: channels have no mid-turn cancel (PARITY.md). We mark
        // intent but cannot abort Claude's in-flight work; the turn still closes
        // when Claude's reply lands. We DO NOT synthesize a 'cancelled' stopReason
        // out of order, which would desync the client.
        dbg(`cancel ${params?.sessionId} (best-effort; channels have no cancel)`)
        break

      // --- graceful stubs for surfaces the channel cannot carry (PARITY.md) ---
      case 'authenticate':
      case 'logout':
        send(acpResult(id, {})) // auth is the live process's OAuth; vacuous ok
        break
      case 'session/load':
        // No history replay; advertised loadSession:false, but answer cleanly if asked.
        send(acpResult(id, null))
        break
      case 'session/set_mode':
        send(acpResult(id, {})) // no channel mode primitive; accepted, no effect
        break

      default:
        if (id !== undefined) send(acpResult(id, {}))
    }
  }

  async function startTurn(id, sessionId, text) {
    const chat_id = String(sessionId)
    // One in-flight turn per session (the channel serializes anyway).
    if (turns.has(chat_id)) {
      // A new prompt while one is open: close the old defensively (shouldn't happen
      // with a well-behaved single-lease client).
      closeTurn(chat_id, STOP_REASONS.END_TURN)
    }
    const timer = setTimeout(() => {
      // Safety net: if Claude never calls finish, don't hang the client forever.
      dbg(`turn ${chat_id} hit timeout; closing with end_turn`)
      send(acpUpdate(chat_id, 'agent_message_chunk', '[channels-kit] turn timed out awaiting completion'))
      closeTurn(chat_id, STOP_REASONS.END_TURN)
    }, turnTimeoutMs)
    turns.set(chat_id, { id, finished: false, timer })
    bus.push({ chat_id, content: text })
    dbg(`turn ${chat_id} started (pushed task)`)
  }

  return { handleLine }
}
