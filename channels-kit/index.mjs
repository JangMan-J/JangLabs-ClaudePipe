// index.mjs — channels-kit public API.
//
// createChannelAgent() runs in the HOST process (claude-pipe's relay child, or a
// standalone host). It: binds the facade↔server transport, spawns the live
// `claude --channels` (lifecycle), wires the ACP facade to a write sink, and
// returns a handle. The CHANNEL SERVER runs in a SEPARATE process that Claude
// spawns as its MCP child (see channel-server-entry.mjs) and dials back into the
// transport this host bound.
//
// Two host shapes are supported by `write`/`input`:
//   - ACP-on-stdio (the claude-pipe recipe): write→process.stdout, feed process.stdin.
//   - embedded (a JS host / tests): provide your own write sink + call handleLine.

import os from 'node:os'
import path from 'node:path'
import fs from 'node:fs'
import { createInterface } from 'node:readline'
import { facadeUnixServer } from './transports.mjs'
import { createAcpFacade } from './acp-facade.mjs'
import { spawnClaudeChannels } from './lifecycle.mjs'

/**
 * Bring up a channels-backed ACP agent.
 *
 * @param {object} opts
 * @param {string} [opts.channelName='cppipe']   The channel/server name.
 * @param {string} [opts.cwd]                     cwd for the live claude.
 * @param {(line:string)=>void} [opts.write]      ACP frame sink (default: stdout).
 * @param {boolean} [opts.readStdin=true]         Feed process.stdin to the facade.
 * @param {object} [opts.permissionPolicy]        See createAcpFacade.
 * @param {string} [opts.serverEntry]             Path to channel-server-entry.mjs
 *                                                (default: sibling file).
 * @returns {Promise<{ facade, claude, sockPath, handleLine, close }>}
 */
export async function createChannelAgent(opts = {}) {
  const {
    channelName = 'cppipe',
    cwd,
    write = (line) => process.stdout.write(line),
    readStdin = true,
    permissionPolicy,
    serverEntry = path.join(path.dirname(new URL(import.meta.url).pathname), 'channel-server-entry.mjs'),
  } = opts

  // 1. Bind the facade↔server transport (the channel server dials in later).
  const sockPath = path.join(os.tmpdir(), `channels-kit-${process.pid}-${channelName}.sock`)
  try {
    fs.unlinkSync(sockPath)
  } catch {}
  const transport = facadeUnixServer(sockPath)

  // 2. Wire the ACP facade to the write sink + the facade-side bus.
  const facade = createAcpFacade({ bus: transport.bus, write, permissionPolicy })

  // 3. Spawn the live claude --channels, pointing it at the channel-server entry.
  //    The entry (Claude's MCP child) reads these env vars to find the socket to
  //    dial back into — so they MUST be set BEFORE we spawn Claude, since the child
  //    inherits this process's environment captured at spawn time.
  process.env.CHANNELS_KIT_SOCK = sockPath
  process.env.CHANNELS_KIT_NAME = channelName
  const claude = await spawnClaudeChannels({
    channelName,
    serverCommand: process.execPath, // node
    serverArgs: [serverEntry],
    cwd,
    onEvent: (e) => process.env.CHANNELS_KIT_DEBUG && process.stderr.write(`[channels-kit] claude ${e}\n`),
  })

  // 4. If hosting ACP-on-stdio, pump stdin → facade.
  if (readStdin) {
    const rl = createInterface({ input: process.stdin })
    rl.on('line', (line) => {
      facade.handleLine(line).catch((e) => process.env.CHANNELS_KIT_DEBUG && process.stderr.write(`[channels-kit] ${e}\n`))
    })
  }

  const close = () => {
    claude.kill()
    transport.close()
    try {
      fs.unlinkSync(sockPath)
    } catch {}
  }
  claude.onExit(() => {
    // If Claude dies, the agent is done.
    process.env.CHANNELS_KIT_DEBUG && process.stderr.write('[channels-kit] claude exited; agent down\n')
  })

  process.on('SIGTERM', () => {
    close()
    process.exit(0)
  })
  process.on('SIGINT', () => {
    close()
    process.exit(0)
  })

  return { facade, claude, sockPath, handleLine: facade.handleLine, close }
}

export { createAcpFacade } from './acp-facade.mjs'
export { startChannelServer } from './channel-server.mjs'
export { spawnClaudeChannels } from './lifecycle.mjs'
export * as protocol from './protocol.mjs'
export * as transports from './transports.mjs'
