// protocol.mjs — the wire constants + tiny pure helpers shared across channels-kit.
//
// Every method-name string here is a VERIFIED Claude Code Channels contract value
// (channels-reference, Claude Code v2.1.186, 2026-06-24). Centralizing them keeps
// the research-preview blast radius in one place: if the contract shifts, this file
// is the single edit point.

/** The channel push: Claude Code registers a listener; we push tasks here. */
export const CHANNEL_PUSH = 'notifications/claude/channel'

/** Inbound permission relay: Claude Code → server when a tool needs approval. */
export const PERMISSION_REQUEST = 'notifications/claude/channel/permission_request'

/** Outbound permission verdict: server → Claude Code, correlated by request_id. */
export const PERMISSION_VERDICT = 'notifications/claude/channel/permission'

/** ACP v1 stop reasons (the prompt-response set; spec §4 / acp-wire-facts). */
export const STOP_REASONS = Object.freeze({
  END_TURN: 'end_turn',
  MAX_TOKENS: 'max_tokens',
  MAX_TURN_REQUESTS: 'max_turn_requests',
  REFUSAL: 'refusal',
  CANCELLED: 'cancelled',
})

/**
 * Sanitize a channel `meta` map: keys MUST be identifiers ([A-Za-z0-9_]); the
 * channel contract SILENTLY DROPS keys with hyphens or other characters, and we
 * surface that by dropping them ourselves (and coercing values to strings) so a
 * caller never wonders why a hyphenated meta key vanished server-side.
 *
 * @param {Record<string, unknown>} meta
 * @returns {Record<string, string>}
 */
export function sanitizeMeta(meta) {
  const out = {}
  for (const [k, v] of Object.entries(meta ?? {})) {
    if (/^[A-Za-z0-9_]+$/.test(k)) out[k] = String(v)
  }
  return out
}

/** Build an ACP session/update notification carrying a text content chunk. */
export function acpUpdate(sessionId, sessionUpdate, text) {
  return {
    jsonrpc: '2.0',
    method: 'session/update',
    params: {
      sessionId,
      update: { sessionUpdate, content: { type: 'text', text } },
    },
  }
}

/** Build an ACP JSON-RPC result response. */
export function acpResult(id, result) {
  return { jsonrpc: '2.0', id, result }
}

/** Build an ACP JSON-RPC error response. */
export function acpError(id, code, message) {
  return { jsonrpc: '2.0', id, error: { code, message } }
}

/**
 * Extract plain text from an ACP session/prompt params.prompt ContentBlock[].
 * Text-only: image/audio/resource blocks are dropped (the channel carries text;
 * PARITY.md documents this). Tolerates a bare {text} or string for robustness.
 */
export function extractPromptText(params) {
  const blocks = params?.prompt
  if (Array.isArray(blocks)) {
    return blocks
      .map((b) => (typeof b === 'string' ? b : b?.text ?? ''))
      .filter(Boolean)
      .join('')
  }
  if (typeof params?.text === 'string') return params.text
  return ''
}
