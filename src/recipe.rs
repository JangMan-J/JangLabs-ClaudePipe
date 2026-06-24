//! Agent recipes — the extension surface (spec §7).
//!
//! A recipe declares how to bring up one kind of warm ACP agent: **spawn command
//! + args, env/auth, pool size, idle policy, and a kind/name for discovery**.
//! Recipes are the *only* place agent-specific or billing-specific knowledge
//! lives; the core stays generic (spec §7, Invariant 8).
//!
//! Per the impl-plan **Orphan ledger**, claude-pipe deliberately ships **no**
//! recipe DSL / config-file schema language — the spec mandates none. Recipes are
//! built-in Rust definitions, the minimal thing. Two recipe *types* are in scope:
//!
//!   - [`RecipeKind::AcpStdio`] (§7.1) — a first-party stdio ACP agent
//!     (`gemini --acp`, `codex`, …): spawn it, pipe its stdio, expose on the data
//!     socket. The clean, caveat-free identity proof.
//!   - [`RecipeKind::ClaudeChannels`] (§7.2) — a live `claude --channels` session
//!     bridged to ACP via a small MCP server. Carries the §7.2 caveats and MUST
//!     NOT contaminate `acp-stdio` data-path purity.

use std::collections::HashMap;

/// Which transport strategy a recipe uses to present an agent on the data socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipeKind {
    /// Spawn a process that speaks raw ACP on stdio; relay its stdio byte-faithful
    /// (the pure case). The data socket is pure ACP (Invariant 7).
    AcpStdio,
    /// Spawn a live `claude --channels` + an MCP `claude/channel` bridge. The
    /// bridge presents a distinct, opt-in protocol on the data socket — NOT raw
    /// ACP — so it never contaminates `acp-stdio` purity (§7.2 architectural note).
    ClaudeChannels,
}

/// One recipe: everything the supervisor needs to spawn + keep one kind of agent.
#[derive(Debug, Clone)]
pub struct Recipe {
    /// Discovery name / kind (spec C): how the orchestrator asks for this agent
    /// ("a claude", "the gemini one"). Also the value persisted in pool state.
    pub name: String,
    /// Transport strategy.
    pub kind: RecipeKind,
    /// The executable to spawn.
    pub command: String,
    /// Arguments passed to the executable.
    pub args: Vec<String>,
    /// Extra environment to set for the child (env/auth — spec §7). Values here
    /// override the inherited environment; a value of `None` *unsets* the key.
    pub env: HashMap<String, Option<String>>,
    /// Warm-pool target size: how many of this agent to keep pre-spawned + idle
    /// (spec §7 pool size; A — warm pool). 0 = spawn-on-demand only.
    pub pool_size: usize,
}

impl Recipe {
    /// Look up a built-in recipe by name. Returns `None` for an unknown name.
    ///
    /// The built-ins cover both in-scope recipe *types* against the agents
    /// verified present on this box (`gemini --acp`, `codex`) plus the strategic
    /// `claude-channels`. Pool sizes are conservative defaults (1) — a recipe
    /// *declares* a size but the spec mandates no particular number.
    pub fn builtin(name: &str) -> Option<Recipe> {
        match name {
            // §7.1 — the primary, caveat-free identity proof. `--acp` is the
            // current flag (verified; `--experimental-acp` is deprecated).
            "gemini" | "acp-stdio" => Some(Recipe {
                name: "gemini".into(),
                kind: RecipeKind::AcpStdio,
                command: "gemini".into(),
                args: vec!["--acp".into()],
                env: HashMap::new(),
                pool_size: 1,
            }),
            // Another real stdio ACP agent on this box (§7.1 lists Codex).
            "codex" => Some(Recipe {
                name: "codex".into(),
                kind: RecipeKind::AcpStdio,
                command: "codex".into(),
                // Codex's ACP entrypoint; kept overridable via a custom recipe if
                // its subcommand differs on a given install.
                args: vec!["acp".into()],
                env: HashMap::new(),
                pool_size: 0,
            }),
            // §7.2 — the strategic, subscription-safe Claude path. The recipe is
            // the blast-radius container for the research-preview churn. It runs
            // the channels bridge (which itself launches `claude --channels`), so
            // the agent rides the subscription. Carries the §7.2 caveats.
            "claude-channels" | "claude" => {
                // Our own bridge (Node, mirroring the proven probe's MCP-SDK
                // usage): it owns the `claude --dangerously-load-development-
                // channels` lifecycle and presents a minimal ACP subset on stdio
                // for the relay. Resolved from `CLAUDE_PIPE_CHANNELS_BRIDGE` or a
                // default repo path. The bridge contains ALL channels research-
                // preview churn (§7.2 blast-radius container) and never touches
                // the acp-stdio data-path code (separate binary → no contamination).
                let bridge = std::env::var("CLAUDE_PIPE_CHANNELS_BRIDGE")
                    .unwrap_or_else(|_| "scripts/claude-channels-bridge.mjs".to_string());
                Some(Recipe {
                    name: "claude-channels".into(),
                    kind: RecipeKind::ClaudeChannels,
                    command: "node".into(),
                    args: vec![bridge],
                    // Subscription OAuth requires ANTHROPIC_API_KEY be UNSET (§7.2).
                    env: HashMap::from([("ANTHROPIC_API_KEY".to_string(), None)]),
                    pool_size: 0,
                })
            }
            // Test-only: a deterministic mock stdio ACP agent for the §12
            // verification suite. Resolved from `CLAUDE_PIPE_MOCK_AGENT` (the path
            // to `tests/support/mock-acp-agent.mjs`); absent that env var the
            // recipe still exists but its spawn will fail loudly. Never used in
            // production — it is the harness for proving the relay, not an agent.
            "mock" => {
                let script = std::env::var("CLAUDE_PIPE_MOCK_AGENT")
                    .unwrap_or_else(|_| "tests/support/mock-acp-agent.mjs".to_string());
                Some(Recipe {
                    name: "mock".into(),
                    kind: RecipeKind::AcpStdio,
                    command: "node".into(),
                    args: vec![script],
                    env: HashMap::new(),
                    pool_size: 0,
                })
            }
            _ => None,
        }
    }

    /// All built-in recipe names (for help / `list`-style discovery).
    pub fn builtin_names() -> &'static [&'static str] {
        &["gemini", "codex", "claude-channels", "mock"]
    }
}
