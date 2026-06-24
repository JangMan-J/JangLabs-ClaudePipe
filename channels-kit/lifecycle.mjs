// lifecycle.mjs — own the live `claude --channels` process (the liveness device).
//
// Claude Code "starts an interactive session by default" but only when it sees a
// TTY; with a plain pipe it drops to --print mode and errors for lack of a prompt.
// So we run Claude under a node-pty PTY purely to keep it interactive and ALIVE
// (§7.2 caveat 3). The PTY carries ONLY Claude's terminal UI, which we discard —
// the task/reply DATA rides the MCP stdio between Claude and the channel server.
// So no terminal emulator sits in the data path (spec Invariant 3): the PTY is a
// liveness device, not a data carrier.
//
// Responsibilities: spawn claude with the channel + mcp-config; auto-confirm the
// one-time "1. I am using this for local development" picker (matcher + timer
// fallback, since the TUI fragments text across escape sequences); detect the
// "channels enabled" banner; keep alive; clean teardown.

import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'

const stripAnsi = (s) =>
  s
    .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, '') // OSC … BEL/ST
    .replace(/\x1b\[[0-9;?<>]*[A-Za-z]/g, '') // CSI (incl. private params)
    .replace(/\x1b[()][AB0]/g, '') // charset selects
    .replace(/\x1b./g, '') // any other 2-char escape

/**
 * Spawn and supervise a live `claude --channels` session pointed at a channel
 * MCP server that runs `serverCommand serverArgs...`.
 *
 * @param {object} opts
 * @param {string} opts.channelName        The server: name (also the MCP key).
 * @param {string} opts.serverCommand      Executable for the channel server (e.g. node).
 * @param {string[]} opts.serverArgs       Args (e.g. [path-to-channel-server-entry]).
 * @param {string} [opts.cwd]              Working dir for claude (fixed for its life).
 * @param {(event: string) => void} [opts.onEvent]  'confirmed' | 'banner' | 'exit'.
 * @returns {{ pid: number, kill: () => void, onExit: (cb) => void, bannerSeen: () => boolean }}
 */
export async function spawnClaudeChannels({ channelName, serverCommand, serverArgs, cwd, onEvent = () => {} }) {
  const dbg = (s) => process.env.CHANNELS_KIT_DEBUG && process.stderr.write(`[lifecycle] ${s}\n`)

  // Temp MCP config naming the channel server. Claude spawns it as a child.
  const mcpConfig = {
    mcpServers: { [channelName]: { command: serverCommand, args: serverArgs } },
  }
  const cfgPath = path.join(os.tmpdir(), `channels-kit-mcp-${process.pid}-${channelName}.json`)
  fs.writeFileSync(cfgPath, JSON.stringify(mcpConfig))

  const { spawn: ptySpawn } = await import('node-pty')
  // server:<name> tag form is required under --dangerously-load-development-channels
  // (Claude rejects a bare name); it points at the MCP server above.
  const claude = ptySpawn(
    'claude',
    ['--dangerously-load-development-channels', `server:${channelName}`, '--mcp-config', cfgPath],
    { name: 'xterm-256color', cols: 200, rows: 50, cwd: cwd || process.cwd(), env: process.env }
  )
  dbg(`spawned claude pid ${claude.pid} channel=${channelName}`)

  let confirmed = false
  let bannerSeen = false
  let win = ''
  // Deterministic fallback: the confirmation picker always appears on first dev-flag
  // use; send "1" after a delay even if the (fragmented) matcher misses it. A stray
  // "1\r" at an idle Claude prompt is harmless.
  const confirmTimer = setTimeout(() => {
    if (!confirmed) {
      confirmed = true
      claude.write('1\r')
      onEvent('confirmed')
      dbg('auto-confirmed (timer fallback)')
    }
  }, 4000)

  claude.onData((data) => {
    const clean = stripAnsi(data)
    win = (win + clean).slice(-6000)
    if (process.env.CHANNELS_KIT_DEBUG) {
      const one = clean.replace(/\s+/g, ' ').trim()
      if (one) process.stderr.write(`[claude-pty] ${one.slice(0, 180)}\n`)
    }
    if (!confirmed && /local development/i.test(win)) {
      confirmed = true
      clearTimeout(confirmTimer)
      setTimeout(() => claude.write('1\r'), 250)
      onEvent('confirmed')
      dbg('auto-confirmed (matched)')
    }
    if (!bannerSeen && /inject\s+directly|experimental/i.test(win)) {
      bannerSeen = true
      onEvent('banner')
      dbg('channels banner seen — listener enabled')
    }
  })

  const exitCbs = []
  claude.onExit(({ exitCode }) => {
    try {
      fs.unlinkSync(cfgPath)
    } catch {}
    onEvent('exit')
    for (const cb of exitCbs) cb(exitCode)
  })

  return {
    pid: claude.pid,
    kill: () => {
      try {
        claude.kill()
      } catch {}
      try {
        fs.unlinkSync(cfgPath)
      } catch {}
    },
    onExit: (cb) => exitCbs.push(cb),
    bannerSeen: () => bannerSeen,
  }
}
