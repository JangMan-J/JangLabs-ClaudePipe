#!/usr/bin/env node
// claude-channels-bridge — the §7.2 strategic recipe's entry point.
//
// As of the channels-kit refactor this is a THIN SHIM: it delegates to the
// channels-kit framework (../channels-kit) running in ACP-on-stdio mode. All the
// real work — the live `claude --channels` lifecycle (PTY/auto-confirm/keep-alive),
// the MCP channel server (claude/channel + permission relay + say/think/finish
// tool surface), and the ACP-subset facade — lives in channels-kit, which is a
// standalone, reusable, tested package. This file remains the path claude-pipe's
// `claude-channels` recipe spawns (`node scripts/claude-channels-bridge.mjs`),
// so the recipe + data-socket contract are unchanged (spec §12.7b stays green).
//
// Why a shim and not a rewrite of the recipe: the recipe (recipe.rs) hardcodes
// this path (overridable via CLAUDE_PIPE_CHANNELS_BRIDGE); keeping the entry here
// means zero Rust changes while the implementation moves into the framework.
//
// channels-kit presents the ACP subset on this process's stdio — the same bytes
// claude-pipe's relay already speaks — as a SEPARATE binary that never imports the
// acp-stdio data-path code (no purity contamination; §7.2 architectural note).
// The honest parity map (what the channel can/cannot carry) is channels-kit/PARITY.md.

import { createChannelAgent } from '../channels-kit/index.mjs'

// Permission policy for the recipe: default to delegating tool-approval prompts is
// not wired to the relay yet (the orchestrator would need an out-of-band answer
// path), so we auto-allow for unattended operation. SECURITY: anyone who can push
// to this agent can thereby approve Claude's tool use — the recipe is for trusted
// orchestrators only (PARITY.md + channels-reference permission-relay caveat).
// Override with CHANNELS_KIT_PERMISSION=deny to refuse all relayed approvals.
const mode = process.env.CHANNELS_KIT_PERMISSION === 'deny' ? 'deny' : 'allow'

await createChannelAgent({
  channelName: process.env.CHANNELS_KIT_NAME || 'cppipe',
  permissionPolicy: { mode },
  readStdin: true, // ACP frames in on stdin, out on stdout
})

process.stderr.write('[claude-channels-bridge] up via channels-kit (ACP subset on stdio; research-preview)\n')
