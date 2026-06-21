//! claude-pipe — a lean, standalone request/reply pipe to a *persistent* Claude
//! Code agent process.
//!
//! Why this exists: spawning `claude -p` per call pays a ~2.4s cold start every
//! time. Instead we spawn ONE long-lived `claude` in streaming-JSON mode and
//! relay messages to it over a Unix-domain socket. The cold start is paid once
//! at `up`; each `send` afterwards costs only Claude's inference latency
//! (~1s warm, measured). No terminal multiplexer in the hot path — we own the
//! process, so we talk to its stdin/stdout directly.
//!
//! The persistent process is driven by Claude's native streaming protocol
//! (verified against claude 2.1.x):
//!
//! - stdin: one JSON object per line —
//!   `{"type":"user","message":{"role":"user","content":"<text>"}}`
//! - stdout: a stream of typed events; each turn ends with a
//!   `{"type":"result","subtype":"success","result":"<text>",...}` event,
//!   which is our turn-completion sentinel.
//! - the same process handles many sequential turns on one `session_id`, so
//!   context is retained across `send`s.
//!
//! Launch flags (required combo): `-p --verbose --input-format stream-json
//! --output-format stream-json`. (`stream-json` output requires `--verbose`.)
//!
//! This is intentionally a thin, reusable PLATFORM: the only thing a consumer
//! needs is `claude-pipe send "<text>"`, which blocks and prints the reply.
//! voxtype's dictation hook is the first consumer, but nothing here is
//! dictation-specific.

mod client;
mod daemon;
mod protocol;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Default session name when `--session` is not given.
const DEFAULT_SESSION: &str = "default";

#[derive(Parser)]
#[command(
    name = "claude-pipe",
    about = "Request/reply pipe to a persistent Claude agent.",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the daemon: spawn the persistent claude process and listen on the
    /// session socket. Runs in the foreground unless `--detach` is given.
    Up {
        /// Session name (one socket + one claude process per name).
        #[arg(long, default_value = DEFAULT_SESSION)]
        session: String,
        /// Model to launch claude with (defaults to claude's own default).
        #[arg(long)]
        model: Option<String>,
        /// System prompt appended to claude's default (keeps replies on-task).
        #[arg(long)]
        system: Option<String>,
        /// Resume a prior conversation by session id (restart recovery).
        #[arg(long)]
        resume: Option<String>,
        /// Use Claude's full agent loadout (tools, MCP, settings/hooks) instead
        /// of the lean default. Needed only for consumers that want Claude to
        /// act, not just transform text. The lean default has much lower
        /// per-turn overhead (no tools/MCP/hooks, replaced system prompt).
        #[arg(long)]
        full: bool,
        /// Fork to the background and return once the socket is ready.
        #[arg(long)]
        detach: bool,
    },
    /// Send one message; block until the turn completes; print the reply.
    Send {
        /// The message text. If omitted, read the message from stdin.
        text: Option<String>,
        #[arg(long, default_value = DEFAULT_SESSION)]
        session: String,
        /// Per-request timeout in milliseconds.
        #[arg(long, default_value_t = 60_000)]
        timeout_ms: u64,
        /// Print the full JSON response envelope instead of just the reply text.
        #[arg(long)]
        json: bool,
    },
    /// Stop the daemon (and its claude process) for a session.
    Down {
        #[arg(long, default_value = DEFAULT_SESSION)]
        session: String,
    },
    /// Report whether a session's daemon is alive and its current session id.
    Status {
        #[arg(long, default_value = DEFAULT_SESSION)]
        session: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Up {
            session,
            model,
            system,
            resume,
            full,
            detach,
        } => daemon::run_up(session, model, system, resume, full, detach).await,
        Cmd::Send {
            text,
            session,
            timeout_ms,
            json,
        } => client::run_send(session, text, timeout_ms, json).await,
        Cmd::Down { session } => client::run_down(session).await,
        Cmd::Status { session } => client::run_status(session).await,
    }
}
