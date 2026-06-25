#!/usr/bin/env node
// channel-server-entry — the process Claude spawns as its MCP/stdio child.
//
// It connects back to the HOST process's unix socket (path + channel name passed
// via env by index.mjs / lifecycle.mjs), then runs the MCP channel server bound to
// that transport. stdout is reserved for the MCP/stdio protocol to Claude; all
// logging goes to stderr.

import { serverUnixClient } from './transports.mjs'
import { startChannelServer } from './channel-server.mjs'

const sockPath = process.env.CHANNELS_KIT_SOCK
const channelName = process.env.CHANNELS_KIT_NAME || 'cppipe'

if (!sockPath) {
  process.stderr.write('[channel-server-entry] CHANNELS_KIT_SOCK not set; cannot connect to host\n')
  process.exit(1)
}

const instructions =
  `Messages arrive as <channel source="${channelName}" chat_id="...">. Each is a task ` +
  'for you to act on. Use the chat_id from the tag in every tool call. ' +
  'Stream your answer with the `say` tool (call it as often as helpful for partial ' +
  'results); optionally share brief progress with `think`; and ALWAYS call `finish` ' +
  'exactly once when the task is complete, passing your final answer.'

// Connect to the host, then start the server on that bus.
const { bus } = await serverUnixClient(sockPath)
await startChannelServer({ name: channelName, instructions, bus })
process.stderr.write(`[channel-server-entry] up; channel '${channelName}', host sock ${sockPath}\n`)
