//! Wire protocol + path helpers shared by the daemon and the client.
//!
//! The client/daemon protocol is deliberately tiny: one JSON `Request` line in,
//! one JSON `Response` line back. That is the entire public contract a consumer
//! depends on.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A request from a client to the daemon: send `text` to the persistent claude
/// and return its reply. One JSON object, newline-terminated, over the socket.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    /// The user message to deliver to claude.
    pub text: String,
    /// Per-request timeout in milliseconds (the daemon aborts the wait, not the
    /// claude process, if a turn exceeds this).
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    60_000
}

/// The daemon's reply to a `Request`. `ok` distinguishes a completed turn from
/// an error/timeout; on error, `text` is empty and `error` explains why.
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    /// Claude's reply text (the `result` field of the stream-json `result`
    /// event), or empty on error.
    #[serde(default)]
    pub text: String,
    /// Claude's session id for this conversation (stable across turns).
    #[serde(default)]
    pub session_id: String,
    /// Wall-clock milliseconds the turn took (daemon-measured).
    #[serde(default)]
    pub turn_ms: u64,
    /// Present only when `ok == false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn error(msg: impl Into<String>) -> Self {
        Response {
            ok: false,
            text: String::new(),
            session_id: String::new(),
            turn_ms: 0,
            error: Some(msg.into()),
        }
    }
}

/// Runtime directory holding per-session sockets and state.
/// `$XDG_RUNTIME_DIR/claude-pipe`, falling back to `/tmp/claude-pipe-$UID`.
pub fn runtime_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("claude-pipe");
        }
    }
    let uid = unsafe { libc_getuid() };
    PathBuf::from(format!("/tmp/claude-pipe-{uid}"))
}

// Tiny libc shim so we don't pull in the whole `libc` crate just for getuid().
extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// The Unix-domain socket path for a session.
pub fn socket_path(session: &str) -> PathBuf {
    runtime_dir().join(format!("{session}.sock"))
}

/// The state file (JSON) for a session: pid + claude session id, used by
/// `status` and for `--resume` recovery.
pub fn state_path(session: &str) -> PathBuf {
    runtime_dir().join(format!("{session}.state.json"))
}

/// Ensure the runtime dir exists with private perms.
pub async fn ensure_runtime_dir() -> Result<()> {
    let dir = runtime_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating runtime dir {}", dir.display()))?;
    Ok(())
}

/// Persisted daemon state, written by the daemon, read by `status`.
#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub pid: u32,
    /// claude's session id once known (after the first turn).
    #[serde(default)]
    pub session_id: Option<String>,
    pub model: Option<String>,
}
