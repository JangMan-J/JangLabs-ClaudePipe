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
 * @param {string} [opts.permissionMode]   `claude --permission-mode` value. Omit to
 *        inherit the user's default (often `bypassPermissions` on a configured box,
 *        in which case NO tool prompts → the permission relay never fires). Pass
 *        `'default'` to make tool use prompt, which is what ENGAGES the
 *        claude/channel/permission relay (the §7.2 "permission prompts relay"
 *        obligation can only be exercised in a prompting mode).
 * @param {(event: string) => void} [opts.onEvent]  'confirmed' | 'banner' | 'exit'.
 * @returns {{ pid: number, kill: () => void, onExit: (cb) => void, bannerSeen: () => boolean }}
 */
export async function spawnClaudeChannels({ channelName, serverCommand, serverArgs, cwd, permissionMode, onEvent = () => {} }) {
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
  const claudeArgs = ['--dangerously-load-development-channels', `server:${channelName}`, '--mcp-config', cfgPath]
  if (permissionMode) claudeArgs.push('--permission-mode', permissionMode)
  const claude = ptySpawn('claude', claudeArgs, {
    name: 'xterm-256color',
    cols: 200,
    rows: 50,
    cwd: cwd || process.cwd(),
    env: process.env,
  })
  dbg(`spawned claude pid ${claude.pid} channel=${channelName}`)

  let confirmed = false
  let bannerSeen = false
  let win = ''
  let torn = false // set on exit/kill so a racing timer is a no-op
  // Deterministic fallback: the confirmation picker always appears on first dev-flag
  // use; send "1" after a delay even if the (fragmented) matcher misses it. A stray
  // "1\r" at an idle Claude prompt is harmless.
  //
  // The timer MUST be cleared on every teardown path (review major: otherwise, if
  // Claude dies within 4s of spawn — the spawn-failure/churn case — this fires after
  // the process is gone, writes '1\r' to a dead PTY, emits a bogus 'confirmed', and
  // (on an embedded host that doesn't process.exit) holds the libuv loop open). We
  // both clearTimeout it in onExit/kill AND guard the body with `torn`, and .unref()
  // it so a pending confirm can never by itself keep an embedded host alive.
  const confirmTimer = setTimeout(() => {
    if (!confirmed && !torn) {
      confirmed = true
      try {
        claude.write('1\r')
      } catch {
        // PTY already gone — nothing to confirm.
      }
      onEvent('confirmed')
      dbg('auto-confirmed (timer fallback)')
    }
  }, 4000)
  if (typeof confirmTimer.unref === 'function') confirmTimer.unref()

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
    torn = true
    clearTimeout(confirmTimer) // review major: don't let the confirm fire post-exit
    try {
      fs.unlinkSync(cfgPath)
    } catch {}
    onEvent('exit')
    for (const cb of exitCbs) cb(exitCode)
  })

  return {
    pid: claude.pid,
    kill: () => {
      torn = true
      clearTimeout(confirmTimer) // review major: clear on the kill() teardown path too
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
