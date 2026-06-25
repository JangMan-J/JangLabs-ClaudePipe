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

// Permission policy for the recipe (CHANNELS_KIT_PERMISSION):
//   'allow'    (default) — auto-approve relayed tool-approval prompts server-side
//                          (unattended).
//   'deny'     — refuse all relayed approvals.
//   'delegate' — surface a REAL ACP session/request_permission to the orchestrator
//                over the data socket and use its verdict (true ACP parity). FAILS
//                CLOSED: if the orchestrator does not answer (or answers with an
//                error/unknown option), the tool is DENIED (see acp-facade.mjs).
// SECURITY: in 'allow', anyone who can push to this agent can thereby approve
// Claude's tool use — the recipe is for trusted orchestrators only (PARITY.md +
// channels-reference permission-relay caveat).
const mode = ['deny', 'delegate'].includes(process.env.CHANNELS_KIT_PERMISSION)
  ? process.env.CHANNELS_KIT_PERMISSION
  : 'allow'

// Permission MODE for the live `claude --channels` (CHANNELS_KIT_PERMISSION_MODE):
// the relay only ENGAGES when Claude actually prompts for tool approval, which it
// does only in a prompting permission mode. The box default is often
// bypassPermissions → Claude never prompts → the relay (and thus 'deny'/'delegate')
// is a no-op. So unless an explicit mode is set, we DEFAULT to 'default' whenever the
// operator chose a non-trivial policy ('deny' or 'delegate'), so their choice has the
// effect they asked for. 'allow' keeps the box default (no forced prompting) — it is
// the unattended auto-approve path and need not prompt. An explicit
// CHANNELS_KIT_PERMISSION_MODE always wins. (Review major: previously the shim never
// forwarded a permission mode at all, so 'delegate' was silently unreachable.)
const explicitMode = process.env.CHANNELS_KIT_PERMISSION_MODE
const permissionMode = explicitMode || (mode === 'allow' ? undefined : 'default')

await createChannelAgent({
  channelName: process.env.CHANNELS_KIT_NAME || 'cppipe',
  permissionPolicy: { mode },
  permissionMode,
  readStdin: true, // ACP frames in on stdin, out on stdout
})

process.stderr.write('[claude-channels-bridge] up via channels-kit (ACP subset on stdio; research-preview)\n')
