//! Wire protocol + path helpers shared across claude-pipe.
//!
//! claude-pipe has **two** protocols, kept rigorously separate:
//!
//! 1. **The data path** — pure ACP (JSON-RPC over a Unix socket), byte-faithful,
//!    **no envelope** (spec Invariant 7). claude-pipe never adds a frame here.
//!    The types for it live nowhere in this file on purpose: the relay treats it
//!    as opaque bytes and parses *only* `sessionId` (see [`acp`]).
//!
//! 2. **The control plane** — the stateless CLI (`list`/`attach`/`spawn`/…) and
//!    the read-only telemetry stream, spoken over a *separate* control socket so
//!    it can never contaminate the data path (spec §6.2). Those request/response
//!    types are [`ControlRequest`] / [`ControlResponse`] below.
//!
//! v1's request/reply (`Request`/`Response`) is retained verbatim so the
//! deployed voxtype dictation tool keeps working during the pivot (spec §12.9);
//! it is deleted only in Phase 7 once v2 verifies.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ===========================================================================
// v1 protocol (RETAINED until Phase 7 — voxtype depends on it; spec §12.9)
// ===========================================================================

/// A v1 request: send `text` to the persistent claude and return its reply.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub text: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    60_000
}

/// The v1 daemon's reply to a [`Request`].
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub turn_ms: u64,
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

// ===========================================================================
// Paths (REUSED transport-agnostic infra — spec §11 REUSE list)
// ===========================================================================

/// Runtime directory holding per-agent data sockets, the control socket, and
/// supervisor state. `$XDG_RUNTIME_DIR/claude-pipe`, falling back to
/// `/tmp/claude-pipe-$UID`.
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

/// v1 per-session socket path (`<session>.sock`). Retained for v1.
pub fn socket_path(session: &str) -> PathBuf {
    runtime_dir().join(format!("{session}.sock"))
}

/// v1 per-session state file. Retained for v1.
pub fn state_path(session: &str) -> PathBuf {
    runtime_dir().join(format!("{session}.state.json"))
}

/// **v2 per-agent ACP data socket** — pure ACP, byte-faithful, one per agent
/// process. Many ACP *sessions* multiplex over this one socket (spec §5).
pub fn agent_socket_path(agent_id: &str) -> PathBuf {
    runtime_dir().join(format!("{agent_id}.acp.sock"))
}

/// **v2 control socket** — the single out-of-band seam the stateless CLI and the
/// telemetry stream speak over. NEVER carries ACP bytes (spec §6.2).
pub fn control_socket_path() -> PathBuf {
    runtime_dir().join("control.sock")
}

/// **v2 supervisor state file** — persists pool membership so `list`/`attach`
/// survive orchestrator restarts (spec §8).
pub fn supervisor_state_path() -> PathBuf {
    runtime_dir().join("supervisor.state.json")
}

/// Ensure the runtime dir exists with private perms.
pub async fn ensure_runtime_dir() -> Result<()> {
    let dir = runtime_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating runtime dir {}", dir.display()))?;
    Ok(())
}

// ===========================================================================
// v1 persisted state (RETAINED) + v2 supervisor state
// ===========================================================================

/// v1 persisted daemon state. Retained for v1.
#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    pub pid: u32,
    #[serde(default)]
    pub session_id: Option<String>,
    pub model: Option<String>,
}

// ===========================================================================
// v2 control-plane protocol (the OUT-OF-BAND CLI ⇄ supervisor seam)
// ===========================================================================

/// Liveness of an agent process, as the supervisor sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Liveness {
    /// Spawned, ACP `initialize` completed, idling in the warm pool.
    Warm,
    /// Spawned but not yet `initialize`-d (cold-start in progress).
    Starting,
    /// The child process has exited.
    Dead,
}

/// A row in the `list` output / a member of the persisted pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Stable agent id (used in the data-socket path and as an `attach` target).
    pub id: String,
    /// The recipe that spawned it (its "kind"/name for discovery — spec C/§7).
    pub recipe: String,
    /// OS pid of the agent child process.
    pub pid: u32,
    /// Current liveness.
    pub liveness: Liveness,
    /// The lease holder's opaque tag, if currently leased (spec §9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_holder: Option<String>,
    /// Number of ACP sessions currently observed multiplexing on this agent.
    #[serde(default)]
    pub session_count: usize,
}

/// A request from the stateless CLI to the supervisor over the control socket.
/// One JSON line in, one [`ControlResponse`] line back — except [`Events`], which
/// streams (see [`ControlResponse::Telemetry`]).
///
/// [`Events`]: ControlRequest::Events
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum ControlRequest {
    /// Enumerate agents (id, recipe/kind, lease holder, liveness). Spec §6.2.
    List,
    /// Grant the caller the exclusive lease on `target` (name|id), performing a
    /// turn-boundary steal if already leased; reply carries the data-socket path.
    Attach {
        target: String,
        /// Opaque caller tag recorded as the lease holder.
        holder: String,
    },
    /// Start a new agent from a registry recipe. Reply carries its [`AgentInfo`].
    Spawn { recipe: String },
    /// Release the caller's lease (on `id` if given, else any lease this holder
    /// owns).
    Detach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        holder: String,
    },
    /// Terminate a pooled agent (SIGTERM). Spec §6.2.
    Kill { id: String },
    /// Subscribe to the read-only telemetry stream for one agent (spec §6.2).
    /// The supervisor replies with a sequence of [`ControlResponse::Telemetry`]
    /// lines until the connection is closed.
    Events { agent: String },
}

/// The supervisor's reply on the control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlResponse {
    /// Generic acknowledgement with a human-readable message.
    Ok { message: String },
    /// Result of `list`.
    Agents { agents: Vec<AgentInfo> },
    /// Result of a successful `attach`: the pure-ACP data socket to connect to.
    Attached { socket: PathBuf, agent: AgentInfo },
    /// Result of a successful `spawn`.
    Spawned { agent: AgentInfo },
    /// One telemetry sample for one ACP session on the watched agent (spec §6.2:
    /// `{session, queue_depth, oldest_unread_ms, lifecycle}`). Streamed.
    Telemetry(Telemetry),
    /// An error reply for any verb.
    Error { message: String },
}

impl ControlResponse {
    pub fn err(msg: impl Into<String>) -> Self {
        ControlResponse::Error {
            message: msg.into(),
        }
    }
}

/// One read-only telemetry sample. Derived **entirely** from the sessionId
/// framing the relay already does — no ACP semantics needed (spec §6.2/§3.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Telemetry {
    /// The ACP `sessionId` this sample is about (or a synthetic marker for
    /// connection-level lifecycle events, e.g. `"-"`).
    pub session: String,
    /// Depth of this session's forward queue (frames buffered toward the client).
    pub queue_depth: usize,
    /// Age in ms of the oldest un-forwarded frame in this session's queue (how
    /// long the orchestrator has left it undrained). 0 if the queue is empty.
    pub oldest_unread_ms: u64,
    /// A lifecycle word: `flowing` | `pressured` (soft bound hit) | `torn`
    /// (lease torn at hard bound) | `agent_dead` | `idle`.
    pub lifecycle: String,
}

// ===========================================================================
// Atomic state I/O (REUSED — spec §11 REUSE list: write_state + with_suffix)
// ===========================================================================

/// Atomically write any serializable state to `path` (temp + rename).
pub async fn write_json_atomic<T: Serialize>(path: &std::path::Path, state: &T) -> Result<()> {
    let tmp = with_suffix(path, ".tmp");
    let json = serde_json::to_vec_pretty(state)?;
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// Append `suffix` to a path's filename.
pub fn with_suffix(path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}
