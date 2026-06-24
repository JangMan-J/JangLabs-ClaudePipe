// Hermetic contract tests for the ACP facade + transport + protocol helpers.
// NO live Claude: we drive the facade-side bus directly and simulate the channel
// server emitting tool calls / permission requests, asserting the ACP frames the
// facade writes. This proves the ACP-subset mapping (plan items 12/13), the
// permission relay correlation (items 6/7), and the meta sanitizer (item 5).

import { test } from 'node:test'
import assert from 'node:assert/strict'
import { inprocPair } from '../transports.mjs'
import { createAcpFacade } from '../acp-facade.mjs'
import { sanitizeMeta, extractPromptText } from '../protocol.mjs'

/** Build a facade wired to an inproc bus; capture written ACP frames. */
function harness(permissionPolicy) {
  const { serverBus, facadeBus } = inprocPair()
  const frames = []
  const facade = createAcpFacade({
    bus: facadeBus,
    write: (line) => frames.push(JSON.parse(line)),
    permissionPolicy,
    turnTimeoutMs: 5000,
  })
  return { facade, frames, serverBus }
}

const line = (o) => JSON.stringify(o)

test('initialize advertises honest capabilities (loadSession:false, text-only)', async () => {
  const { facade, frames } = harness()
  await facade.handleLine(line({ jsonrpc: '2.0', id: 1, method: 'initialize', params: {} }))
  const r = frames.find((f) => f.id === 1)
  assert.equal(r.result.protocolVersion, 1)
  assert.equal(r.result.agentCapabilities.loadSession, false)
  assert.equal(r.result.agentCapabilities.promptCapabilities.image, false)
})

test('session/new mints a sessionId', async () => {
  const { facade, frames } = harness()
  await facade.handleLine(line({ jsonrpc: '2.0', id: 2, method: 'session/new', params: {} }))
  const r = frames.find((f) => f.id === 2)
  assert.match(r.result.sessionId, /^chan-\d+$/)
})

test('prompt → say/think stream as chunks → finish closes with end_turn', async () => {
  const { facade, frames, serverBus } = harness()
  await facade.handleLine(line({ jsonrpc: '2.0', id: 3, method: 'session/new', params: {} }))
  const sid = frames.find((f) => f.id === 3).result.sessionId

  // Capture the push the facade emits, then simulate Claude's tool calls.
  let pushed = null
  serverBus.onPush((p) => (pushed = p))

  const promptDone = facade.handleLine(
    line({ jsonrpc: '2.0', id: 4, method: 'session/prompt', params: { sessionId: sid, prompt: [{ type: 'text', text: 'hello' }] } })
  )
  // Give the push a tick.
  await new Promise((r) => setTimeout(r, 10))
  assert.ok(pushed, 'facade pushed a task')
  assert.equal(pushed.chat_id, sid)
  assert.equal(pushed.content, 'hello')

  // Simulate streaming: two say chunks, one think, then finish.
  serverBus.emitToolCall({ chat_id: sid, tool: 'think', args: { text: 'pondering' } })
  serverBus.emitToolCall({ chat_id: sid, tool: 'say', args: { text: 'Hel' } })
  serverBus.emitToolCall({ chat_id: sid, tool: 'say', args: { text: 'lo!' } })
  serverBus.emitToolCall({ chat_id: sid, tool: 'finish', args: { text: 'Hello!' } })
  await promptDone
  await new Promise((r) => setTimeout(r, 10))

  const updates = frames.filter((f) => f.method === 'session/update')
  const thoughts = updates.filter((f) => f.params.update.sessionUpdate === 'agent_thought_chunk')
  const messages = updates.filter((f) => f.params.update.sessionUpdate === 'agent_message_chunk')
  assert.equal(thoughts.length, 1, 'one thought chunk')
  assert.equal(thoughts[0].params.update.content.text, 'pondering')
  // two streamed says + the finish's final text
  assert.deepEqual(messages.map((m) => m.params.update.content.text), ['Hel', 'lo!', 'Hello!'])

  const close = frames.find((f) => f.id === 4 && f.result?.stopReason)
  assert.equal(close.result.stopReason, 'end_turn')
})

test('permission_request → verdict (allow policy)', async () => {
  const { facade, serverBus } = harness({ mode: 'allow' })
  let verdict = null
  serverBus.onPermissionVerdict((v) => (verdict = v))
  serverBus.emitPermissionRequest({ request_id: 'abcde', tool_name: 'Bash', description: 'run ls', input_preview: '{"cmd":"ls"}' })
  await new Promise((r) => setTimeout(r, 10))
  assert.equal(verdict.request_id, 'abcde')
  assert.equal(verdict.behavior, 'allow')
})

test('permission_request → deny policy', async () => {
  const { facade, serverBus } = harness({ mode: 'deny' })
  let verdict = null
  serverBus.onPermissionVerdict((v) => (verdict = v))
  serverBus.emitPermissionRequest({ request_id: 'qwxyz', tool_name: 'Write', description: 'write f', input_preview: '{}' })
  await new Promise((r) => setTimeout(r, 10))
  assert.equal(verdict.behavior, 'deny')
})

test('delegate mode: client allowing the ACP request yields an allow verdict', async () => {
  // The primary delegate path is ACP-first (the separate test below covers a
  // reject → deny); here we confirm allow round-trips and that the ACP request
  // carries the channel's request_id as the toolCallId for correlation.
  const { facade, frames, serverBus } = harness({ mode: 'delegate' })
  let verdict = null
  serverBus.onPermissionVerdict((v) => (verdict = v))
  serverBus.emitPermissionRequest({ request_id: 'ccccc', tool_name: 'Bash', description: '', input_preview: '' })
  await new Promise((r) => setTimeout(r, 5))
  const acpReq = frames.find((f) => f.method === 'session/request_permission' && f.params.toolCall.toolCallId === 'ccccc')
  assert.ok(acpReq, 'ACP request emitted with the channel request_id')
  await facade.handleLine(line({ jsonrpc: '2.0', id: acpReq.id, result: { outcome: { outcome: 'selected', optionId: 'allow_once' } } }))
  await new Promise((r) => setTimeout(r, 5))
  assert.equal(verdict.behavior, 'allow')
})

test('graceful stubs: load/set_mode/authenticate do not hang', async () => {
  const { facade, frames } = harness()
  await facade.handleLine(line({ jsonrpc: '2.0', id: 10, method: 'session/load', params: { sessionId: 'x' } }))
  await facade.handleLine(line({ jsonrpc: '2.0', id: 11, method: 'session/set_mode', params: { sessionId: 'x', modeId: 'code' } }))
  await facade.handleLine(line({ jsonrpc: '2.0', id: 12, method: 'authenticate', params: {} }))
  assert.ok(frames.find((f) => f.id === 10 && 'result' in f))
  assert.ok(frames.find((f) => f.id === 11 && 'result' in f))
  assert.ok(frames.find((f) => f.id === 12 && 'result' in f))
})

test('meta sanitizer drops non-identifier keys, coerces values', () => {
  const out = sanitizeMeta({ chat_id: 1, 'bad-key': 'x', good_1: 'y', 'no.dots': 'z' })
  assert.deepEqual(out, { chat_id: '1', good_1: 'y' })
})

test('extractPromptText flattens text blocks, drops non-text', () => {
  assert.equal(extractPromptText({ prompt: [{ type: 'text', text: 'a' }, { type: 'image', data: '...' }, { type: 'text', text: 'b' }] }), 'ab')
  assert.equal(extractPromptText({ text: 'legacy' }), 'legacy')
  assert.equal(extractPromptText({ prompt: [] }), '')
})

// --- post-audit regressions ------------------------------------------------

async function openTurn(facade, frames, serverBus, promptId) {
  await facade.handleLine(line({ jsonrpc: '2.0', id: promptId * 10, method: 'session/new', params: {} }))
  const sid = frames.find((f) => f.id === promptId * 10).result.sessionId
  facade.handleLine(line({ jsonrpc: '2.0', id: promptId, method: 'session/prompt', params: { sessionId: sid, prompt: [{ type: 'text', text: 'q' }] } }))
  await new Promise((r) => setTimeout(r, 5))
  return sid
}

test('H2: session/cancel makes the turn close with stopReason cancelled', async () => {
  const { facade, frames, serverBus } = harness()
  const sid = await openTurn(facade, frames, serverBus, 7)
  await facade.handleLine(line({ jsonrpc: '2.0', method: 'session/cancel', params: { sessionId: sid } }))
  // Claude eventually calls finish — but because cancel was requested, the close
  // must report 'cancelled', not 'end_turn'.
  serverBus.emitToolCall({ chat_id: sid, tool: 'finish', args: { text: 'done' } })
  await new Promise((r) => setTimeout(r, 5))
  const close = frames.find((f) => f.id === 7 && f.result?.stopReason)
  assert.equal(close.result.stopReason, 'cancelled')
})

test('H2: id-bearing session/cancel is acknowledged (no orphaned id)', async () => {
  const { facade, frames, serverBus } = harness()
  const sid = await openTurn(facade, frames, serverBus, 8)
  await facade.handleLine(line({ jsonrpc: '2.0', id: 999, method: 'session/cancel', params: { sessionId: sid } }))
  assert.ok(frames.find((f) => f.id === 999 && 'result' in f), 'id-bearing cancel acked')
})

test('H3: a mis-tagged tool_call is routed to the single open turn (not dropped/misrouted)', async () => {
  const { facade, frames, serverBus } = harness()
  const sid = await openTurn(facade, frames, serverBus, 5)
  // Claude tags the say with a WRONG chat_id; with one turn open it must still
  // reach this session (fail-closed routing), and never leak elsewhere.
  serverBus.emitToolCall({ chat_id: 'totally-wrong-id', tool: 'say', args: { text: 'answer' } })
  await new Promise((r) => setTimeout(r, 5))
  const chunk = frames.find((f) => f.method === 'session/update' && f.params.sessionId === sid)
  assert.ok(chunk, 'mis-tagged say still reached the single open turn')
  assert.equal(chunk.params.update.content.text, 'answer')
})

test('C4: unknown id-bearing method gets method-not-found, not fake success', async () => {
  const { facade, frames } = harness()
  await facade.handleLine(line({ jsonrpc: '2.0', id: 77, method: 'totally/unknown', params: {} }))
  const r = frames.find((f) => f.id === 77)
  assert.ok(r.error, 'error response, not result')
  assert.equal(r.error.code, -32601)
})

test('B2: failAllTurns closes every open turn with cancelled', async () => {
  const { facade, frames, serverBus } = harness()
  const s1 = await openTurn(facade, frames, serverBus, 1)
  const s2 = await openTurn(facade, frames, serverBus, 2)
  facade.failAllTurns()
  await new Promise((r) => setTimeout(r, 5))
  const c1 = frames.find((f) => f.id === 1 && f.result?.stopReason)
  const c2 = frames.find((f) => f.id === 2 && f.result?.stopReason)
  assert.equal(c1.result.stopReason, 'cancelled')
  assert.equal(c2.result.stopReason, 'cancelled')
})

test('delegate mode emits a real ACP session/request_permission and maps the outcome', async () => {
  const { facade, frames, serverBus } = harness({ mode: 'delegate' })
  await openTurn(facade, frames, serverBus, 3)
  let verdict = null
  serverBus.onPermissionVerdict((v) => (verdict = v))
  serverBus.emitPermissionRequest({ request_id: 'reqid', tool_name: 'Bash', description: 'ls', input_preview: '{}' })
  await new Promise((r) => setTimeout(r, 5))
  // The facade should have emitted an ACP session/request_permission request.
  const acpReq = frames.find((f) => f.method === 'session/request_permission')
  assert.ok(acpReq, 'real ACP session/request_permission emitted')
  assert.equal(acpReq.params.toolCall.toolCallId, 'reqid')
  assert.ok(Array.isArray(acpReq.params.options))
  // Answer it as the client would (reject) → channel verdict must be deny.
  await facade.handleLine(line({ jsonrpc: '2.0', id: acpReq.id, result: { outcome: { outcome: 'selected', optionId: 'reject_once' } } }))
  await new Promise((r) => setTimeout(r, 5))
  assert.equal(verdict.behavior, 'deny')
})
