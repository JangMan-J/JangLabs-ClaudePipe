//! claude-pipe — v1: a lean request/reply pipe to a persistent Claude Code agent
//! (retained for the deployed voxtype dictation tool). v2: a semantics-blind ACP
//! **transport** for a model-as-client orchestrator (the pivot — see
//! `docs/acp-transport-spec.md`).
//!
//! The two coexist during the pivot (spec §11/§12.9): v1's `up`/`send`/`down`/
//! `status` keep working until v2 verifies. v2 adds a supervisor (`serve`) that
//! owns a warm pool of ACP agents and a stateless control CLI
//! (`list`/`attach`/`spawn`/`detach`/`kill`/`events`).
//!
//! ── v1 persistent-Claude protocol (verified against claude 2.1.x) ──
//! stdin: `{"type":"user","message":{"role":"user","content":"<text>"}}` per
//! line; stdout: typed events ending each turn with a `result` event. v1 launch
//! combo: `-p --verbose --input-format stream-json --output-format stream-json`.
//!
//! ── v2 ACP transport ── each agent's raw ACP stdio is exposed byte-faithfully
//! over a per-agent Unix socket; the orchestrator points a stock ACP client at
//! the path that `attach` prints. claude-pipe parses only `sessionId` + the
//! prompt/stopReason turn bracket and is otherwise byte-transparent.

mod acp;
mod client;
mod control;
mod daemon;
mod protocol;
mod recipe;
mod relay;
mod supervisor;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Default session name when `--session` is not given (v1).
const DEFAULT_SESSION: &str = "default";

#[derive(Parser)]
#[command(
    name = "claude-pipe",
    about = "v1: request/reply pipe to a persistent Claude agent. v2: ACP transport for an orchestrator.",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    // ───────────────────────── v1 (retained) ─────────────────────────
    /// [v1] Start the persistent-claude daemon and listen on the session socket.
    Up {
        #[arg(long, default_value = DEFAULT_SESSION)]
        session: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        system: Option<String>,
        #[arg(long)]
        resume: Option<String>,
        #[arg(long)]
        full: bool,
        #[arg(long)]
        detach: bool,
    },
    /// [v1] Send one message; block until the turn completes; print the reply.
    Send {
        text: Option<String>,
        #[arg(long, default_value = DEFAULT_SESSION)]
        session: String,
        #[arg(long, default_value_t = 60_000)]
        timeout_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// [v1] Stop the persistent-claude daemon for a session.
    Down {
        #[arg(long, default_value = DEFAULT_SESSION)]
        session: String,
    },
    /// [v1] Report whether a session's daemon is alive and its session id.
    Status {
        #[arg(long, default_value = DEFAULT_SESSION)]
        session: String,
    },

    // ───────────────────────── v2 (ACP transport) ─────────────────────────
    /// [v2] Run the supervisor: own a warm pool of ACP agents + serve the control
    /// socket. Pre-spawn agents by passing recipe names. Runs in the foreground
    /// unless `--detach`.
    Serve {
        /// Recipes to pre-spawn into the warm pool (repeatable), e.g.
        /// `--prespawn gemini --prespawn gemini`.
        #[arg(long = "prespawn")]
        prespawn: Vec<String>,
        /// Fork to the background and return once the control socket is ready.
        #[arg(long)]
        detach: bool,
    },
    /// [v2] List pooled agents (id, recipe, lease holder, liveness, sessions).
    List {
        #[arg(long)]
        json: bool,
    },
    /// [v2] Start a new agent from a recipe.
    Spawn {
        /// Recipe name (e.g. `gemini`, `codex`, `claude-channels`).
        recipe: String,
        #[arg(long)]
        json: bool,
    },
    /// [v2] Grant the lease on an agent and PRINT its data-socket path.
    Attach {
        /// Agent name (recipe kind) or id, e.g. `gemini` or `gemini-1` or `#1`.
        target: String,
        /// Opaque lease-holder tag (defaults to a per-shell tag).
        #[arg(long)]
        holder: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// [v2] Release the caller's lease (on `--id` if given, else any it holds).
    Detach {
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        holder: Option<String>,
    },
    /// [v2] Terminate a pooled agent (SIGTERM).
    Kill {
        /// Agent id.
        id: String,
    },
    /// [v2] Stream read-only telemetry for one agent until interrupted.
    Events {
        #[arg(long)]
        agent: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        // v1
        Cmd::Up { session, model, system, resume, full, detach } => {
            daemon::run_up(session, model, system, resume, full, detach).await
        }
        Cmd::Send { text, session, timeout_ms, json } => {
            client::run_send(session, text, timeout_ms, json).await
        }
        Cmd::Down { session } => client::run_down(session).await,
        Cmd::Status { session } => client::run_status(session).await,
        // v2
        Cmd::Serve { prespawn, detach } => {
            if detach {
                supervisor_spawn_detached(prespawn).await
            } else {
                supervisor::run_supervisor(prespawn).await
            }
        }
        Cmd::List { json } => control::run_list(json).await,
        Cmd::Spawn { recipe, json } => control::run_spawn(recipe, json).await,
        Cmd::Attach { target, holder, json } => control::run_attach(target, holder, json).await,
        Cmd::Detach { id, holder } => control::run_detach(id, holder).await,
        Cmd::Kill { id } => control::run_kill(id).await,
        Cmd::Events { agent } => control::run_events(agent).await,
    }
}

/// Background-spawn a fresh `claude-pipe serve` (without --detach), reusing v1's
/// setsid ceremony (spec §8: detach from the launching shell), and wait for the
/// control socket. Used by `serve --detach`.
async fn supervisor_spawn_detached(prespawn: Vec<String>) -> Result<()> {
    use anyhow::{anyhow, Context};
    use std::process::Stdio;
    use tokio::net::UnixStream;
    use tokio::time::{Duration, Instant};

    protocol::ensure_runtime_dir().await?;
    let exe = std::env::current_exe().context("locating own exe")?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("serve");
    for r in &prespawn {
        cmd.arg("--prespawn").arg(r);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            // Reuse v1's setsid ceremony so the supervisor survives the shell.
            if daemon::libc_setsid_exposed() == -1 {
                // Non-fatal: still usable if already a session leader.
            }
            Ok(())
        });
    }
    let _child = cmd.spawn().context("spawning detached supervisor")?;

    let sock = protocol::control_socket_path();
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if UnixStream::connect(&sock).await.is_ok() {
            println!("claude-pipe: supervisor up (detached)");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow!(
        "detached supervisor did not become ready within 15s (control socket {})",
        sock.display()
    ))
}
