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

test('permission delegate policy consults the host', async () => {
  const calls = []
  const { facade, serverBus } = harness({
    mode: 'delegate',
    onRequest: async (req) => {
      calls.push(req)
      return req.tool_name === 'Bash' ? 'allow' : 'deny'
    },
  })
  const verdicts = []
  serverBus.onPermissionVerdict((v) => verdicts.push(v))
  serverBus.emitPermissionRequest({ request_id: 'aaaaa', tool_name: 'Bash', description: '', input_preview: '' })
  serverBus.emitPermissionRequest({ request_id: 'bbbbb', tool_name: 'Write', description: '', input_preview: '' })
  await new Promise((r) => setTimeout(r, 20))
  assert.equal(calls.length, 2)
  assert.equal(verdicts.find((v) => v.request_id === 'aaaaa').behavior, 'allow')
  assert.equal(verdicts.find((v) => v.request_id === 'bbbbb').behavior, 'deny')
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
