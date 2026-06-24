//! The per-agent relay: a byte-faithful, semantics-blind, full-duplex ACP
//! conduit between a single leased client (the orchestrator) and one warm agent
//! child process.
//!
//! This is the load-bearing module — almost every spec invariant lives here:
//!
//!   - **Invariant 1 (byte-faithful):** both directions relay original bytes
//!     verbatim. We split the agent's stdout into newline-delimited frames only
//!     to read `sessionId`; we forward the *original frame bytes*, never a
//!     re-serialized form. Two independent, mutually non-blocking loops.
//!   - **Invariant 2 (semantics-blind):** the only parse is [`crate::acp`] —
//!     `sessionId` + the `prompt`/`stopReason` turn bracket. Nothing else.
//!   - **§6.3 fairness + OPEN-1 overflow:** demux agent→client frames by
//!     `sessionId` into per-session bounded forward queues; **continuously drain
//!     the agent's shared stdout always** (never halt the read); apply
//!     backpressure only on the forward side; at a session's **soft bound** stop
//!     forwarding that session and surface it "pressured"; at a **hard memory
//!     bound** tear the lease with a logged reason. Drop-oldest is forbidden.
//!   - **Invariant 5 (never-silent):** every lease teardown / drop is logged and
//!     surfaced on telemetry with a reason.
//!   - **§9 lease & steal:** single-client exclusive lease; an attach steals the
//!     lease, but only at a turn boundary (no `session/prompt` in flight).
//!   - **Invariant 9 (strictly in-band):** this module opens **no** fd except the
//!     agent's stdio (owned by the supervisor) and the client socket handed to it.
//!     There is no filesystem/log/artifact read anywhere in the data path.

use crate::acp::{self, RpcId};
use crate::protocol::Telemetry;
use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::Instant;

/// Per-session forward-queue **soft** bound: how many frames may buffer toward
/// the client before we stop forwarding that session and mark it "pressured"
/// (still draining the agent; other sessions unaffected). Surfaced, not fatal.
const SOFT_BOUND_FRAMES: usize = 1024;

/// Per-session forward-queue **hard memory** bound in bytes. Only when a single
/// pressured session's buffered bytes exceed this does the relay tear the lease
/// (with a logged reason). Bounds memory while staying byte-faithful — a torn
/// lease loses the *whole* stream cleanly, never corrupts a live one (§6.3).
const HARD_BOUND_BYTES: usize = 64 * 1024 * 1024;

/// A command the supervisor sends to a running relay over its control channel.
pub enum RelayCmd {
    /// Hand a freshly-accepted client connection to this relay, taking the lease
    /// (a turn-boundary steal if one is held). `holder` is the lease-holder tag.
    /// Replies with the data-socket path having been granted (Ok) or an error.
    Attach {
        stream: UnixStream,
        holder: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Release the lease if `holder` currently holds it.
    Detach {
        holder: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Subscribe to this agent's telemetry stream; samples are pushed on `tx`.
    Subscribe { tx: mpsc::Sender<Telemetry> },
    /// Report the current lease holder (for `list`).
    LeaseHolder {
        reply: oneshot::Sender<Option<String>>,
    },
    /// Count of distinct sessions currently tracked (for `list`).
    SessionCount { reply: oneshot::Sender<usize> },
    /// Pre-register the holder a forthcoming data-socket connection should be
    /// leased under. The control-plane `attach` verb (which hands the path back)
    /// calls this so the lease holder shown by `list` and matched by `detach` is
    /// the orchestrator's real tag — not the generic data-socket placeholder.
    SetIntendedHolder { holder: String },
}

/// Shared, mutable relay state guarded by a single mutex. Kept small: the demux
/// bookkeeping, the lease, and the turn tracker. The hot byte loops touch it
/// only at frame boundaries (cheap), never mid-frame.
struct RelayState {
    /// Per-session forward queues (agent→client), drained fairly. Each entry is
    /// the original frame bytes plus when it was enqueued (for oldest_unread_ms).
    queues: HashMap<String, SessionQueue>,
    /// Round-robin cursor over session ids, so no session starves another.
    drain_order: VecDeque<String>,
    /// In-flight `session/prompt` ids → their sessionId. Presence of any entry
    /// for a session means a turn is open there (steal-unsafe). Closed when the
    /// matching `stopReason` response id arrives.
    open_turns: HashMap<RpcId, String>,
    /// Telemetry subscribers (the `events --agent` streams).
    telemetry: Vec<mpsc::Sender<Telemetry>>,
    /// Set when a session's hard memory bound was exceeded: the forwarder reads
    /// this and closes the client (lease torn cleanly; §6.3 hard bound).
    torn: bool,
}

/// One session's forward queue plus bound bookkeeping.
struct SessionQueue {
    frames: VecDeque<(Vec<u8>, Instant)>,
    bytes: usize,
    /// True once the soft bound was hit and we stopped forwarding this session.
    pressured: bool,
}

impl SessionQueue {
    fn new() -> Self {
        SessionQueue {
            frames: VecDeque::new(),
            bytes: 0,
            pressured: false,
        }
    }
    fn push(&mut self, frame: Vec<u8>) {
        self.bytes += frame.len();
        self.frames.push_back((frame, Instant::now()));
    }
    fn pop(&mut self) -> Option<Vec<u8>> {
        let (f, _) = self.frames.pop_front()?;
        self.bytes -= f.len();
        Some(f)
    }
    fn oldest_age_ms(&self) -> u64 {
        self.frames
            .front()
            .map(|(_, t)| t.elapsed().as_millis() as u64)
            .unwrap_or(0)
    }
}

impl RelayState {
    fn new() -> Self {
        RelayState {
            queues: HashMap::new(),
            drain_order: VecDeque::new(),
            open_turns: HashMap::new(),
            telemetry: Vec::new(),
            torn: false,
        }
    }

    /// True iff any turn is open for `session` (a `session/prompt` awaiting its
    /// `stopReason`). The single steal-safety predicate (§9).
    fn any_turn_open(&self) -> bool {
        !self.open_turns.is_empty()
    }

    /// Emit a telemetry sample to all subscribers (best-effort; a full/closed
    /// subscriber is dropped). Never-silent: lease tears and pressure transitions
    /// all flow through here.
    fn emit(&mut self, t: Telemetry) {
        self.telemetry
            .retain(|tx| tx.try_send(t.clone()).is_ok() || !tx.is_closed());
    }
}

/// A running relay handle the supervisor keeps. Owns the command channel.
pub struct RelayHandle {
    pub cmd_tx: mpsc::Sender<RelayCmd>,
}

/// Spawn the relay for one agent. Takes ownership of the agent child's piped
/// stdin/stdout (the supervisor spawned the child and hands us the fds). Returns
/// a handle the supervisor uses to route attaches/detaches/telemetry.
///
/// The relay runs until the agent's stdout closes (process death) or it is
/// dropped. It does **not** own the child handle itself — the supervisor reaps it.
pub fn spawn_relay(agent_id: String, stdin: ChildStdin, stdout: ChildStdout) -> RelayHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<RelayCmd>(32);
    tokio::spawn(async move {
        if let Err(e) = relay_main(agent_id.clone(), stdin, stdout, cmd_rx).await {
            eprintln!("claude-pipe: relay for agent '{agent_id}' ended: {e:#}");
        }
    });
    RelayHandle { cmd_tx }
}

/// The relay's owned state: the agent stdio, the lease, and the shared demux.
async fn relay_main(
    agent_id: String,
    agent_stdin: ChildStdin,
    agent_stdout: ChildStdout,
    mut cmd_rx: mpsc::Receiver<RelayCmd>,
) -> Result<()> {
    let state = Arc::new(Mutex::new(RelayState::new()));

    // The agent's stdin is written by whichever client currently holds the lease.
    // We funnel client→agent bytes through this single writer task so concurrent
    // attaches never interleave a half-written frame onto the agent's stdin.
    let agent_stdin = Arc::new(Mutex::new(agent_stdin));

    // Continuously-draining agent→client demux task. This NEVER stops reading the
    // agent's shared stdout (OPEN-1 rule 1) — it only ever buffers onto per-session
    // forward queues. It also tracks turn open/close as frames pass.
    let demux_state = state.clone();
    let demux_id = agent_id.clone();
    let demux = tokio::spawn(async move {
        agent_to_queues(demux_id, agent_stdout, demux_state).await;
    });

    // The current lease: a channel to tell the active client-writer task to stop
    // (used on steal/detach), plus the holder tag.
    let mut lease: Option<Lease> = None;
    // The holder a forthcoming raw data-socket connection should be leased under,
    // set by the control-plane `attach` (SetIntendedHolder) just before it returns
    // the path. A connection arriving with the generic "data-socket" tag adopts
    // this instead, so `list`/`detach` see the orchestrator's real holder.
    let mut intended_holder: Option<String> = None;

    loop {
        tokio::select! {
            // Agent stdout closed → relay is done (demux task returns).
            _ = wait_demux_done(&demux) => {
                let mut st = state.lock().await;
                st.emit(Telemetry { session: "-".into(), queue_depth: 0, oldest_unread_ms: 0, lifecycle: "agent_dead".into() });
                break;
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    RelayCmd::Attach { stream, holder, reply } => {
                        // A generic data-socket connection adopts the intended
                        // holder pre-registered by the control-plane attach.
                        let effective = if holder == "data-socket" {
                            intended_holder.take().unwrap_or(holder)
                        } else {
                            holder
                        };
                        let r = do_attach(&agent_id, &state, &agent_stdin, &mut lease, stream, effective).await;
                        let _ = reply.send(r);
                    }
                    RelayCmd::Detach { holder, reply } => {
                        let r = do_detach(&mut lease, &holder);
                        let _ = reply.send(r);
                    }
                    RelayCmd::Subscribe { tx } => {
                        state.lock().await.telemetry.push(tx);
                    }
                    RelayCmd::LeaseHolder { reply } => {
                        let _ = reply.send(lease.as_ref().map(|l| l.holder.clone()));
                    }
                    RelayCmd::SessionCount { reply } => {
                        let _ = reply.send(state.lock().await.queues.len());
                    }
                    RelayCmd::SetIntendedHolder { holder } => {
                        intended_holder = Some(holder);
                    }
                }
            }
        }
    }

    demux.abort();
    Ok(())
}

/// An active lease: the holder tag and a kill switch for its client-writer task.
struct Lease {
    holder: String,
    /// Dropping/sending stops the client→agent writer + the queue-forwarder for
    /// the old client (on steal or detach).
    stop: Option<oneshot::Sender<()>>,
}

/// Grant the lease to a new client, stealing at a turn boundary if needed (§9).
async fn do_attach(
    agent_id: &str,
    state: &Arc<Mutex<RelayState>>,
    agent_stdin: &Arc<Mutex<ChildStdin>>,
    lease: &mut Option<Lease>,
    stream: UnixStream,
    holder: String,
) -> Result<(), String> {
    // Steal safety: a live steal is permitted ONLY when no session/prompt is in
    // flight (§9). If a turn is open, wait (bounded) for it to close. This is the
    // *only* semantic peek the lease costs.
    if lease.is_some() {
        let waited = wait_turn_boundary(state).await;
        if !waited {
            return Err(
                "steal refused: a turn stayed open past the handoff wait window".into(),
            );
        }
        // Drop the old lease — closes the old client's socket and stops its tasks.
        if let Some(old) = lease.take() {
            if let Some(stop) = old.stop {
                let _ = stop.send(());
            }
        }
    }

    // Wire the new client: split the socket, start its two directions.
    let (stop_tx, stop_rx) = oneshot::channel();
    let (read_half, write_half) = stream.into_split();

    // client→agent: forward raw bytes to the single agent-stdin writer, tracking
    // turn opens as session/prompt requests pass.
    let c2a_state = state.clone();
    let c2a_stdin = agent_stdin.clone();
    let c2a_id = agent_id.to_string();
    tokio::spawn(async move {
        client_to_agent(c2a_id, read_half, c2a_stdin, c2a_state).await;
    });

    // agent→client: drain the per-session forward queues fairly to this client,
    // honoring soft/hard bounds. Stops when `stop_rx` fires (steal/detach) or the
    // client hangs up.
    let a2c_state = state.clone();
    let a2c_id = agent_id.to_string();
    tokio::spawn(async move {
        queues_to_client(a2c_id, write_half, a2c_state, stop_rx).await;
    });

    *lease = Some(Lease {
        holder,
        stop: Some(stop_tx),
    });
    Ok(())
}

fn do_detach(lease: &mut Option<Lease>, holder: &str) -> Result<(), String> {
    match lease {
        Some(l) if l.holder == holder => {
            if let Some(old) = lease.take() {
                if let Some(stop) = old.stop {
                    let _ = stop.send(());
                }
            }
            Ok(())
        }
        Some(l) => Err(format!("lease held by '{}', not '{holder}'", l.holder)),
        None => Err("no lease to detach".into()),
    }
}

/// Wait until no turn is open (steal-safe), bounded so a wedged turn can't block
/// a handoff forever. Returns true if a boundary was reached, false on timeout.
async fn wait_turn_boundary(state: &Arc<Mutex<RelayState>>) -> bool {
    // A handoff MAY briefly wait for the current turn's stopReason (§9).
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if !state.lock().await.any_turn_open() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Block until the demux task has finished (agent stdout closed). We can't `await`
/// a `&JoinHandle`, so poll its finished flag cheaply.
async fn wait_demux_done(demux: &tokio::task::JoinHandle<()>) {
    while !demux.is_finished() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// **Agent→queues**: continuously read the agent's shared stdout, split into
/// frames, demux by `sessionId`, push onto per-session forward queues, and track
/// turn open/close. NEVER halts the read (OPEN-1 rule 1). Returns when stdout
/// closes (agent death).
async fn agent_to_queues(
    agent_id: String,
    mut stdout: ChildStdout,
    state: Arc<Mutex<RelayState>>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = match stdout.read(&mut chunk).await {
            Ok(0) => return, // EOF: agent process gone
            Ok(n) => n,
            Err(e) => {
                eprintln!("claude-pipe: agent '{agent_id}' stdout read error: {e}");
                return;
            }
        };
        buf.extend_from_slice(&chunk[..n]);

        // Pull every complete frame out of the buffer.
        let (frames, consumed): (Vec<Vec<u8>>, usize) = {
            let (refs, consumed) = acp::split_frames(&buf);
            (refs.into_iter().map(|f| f.to_vec()).collect(), consumed)
        };
        if consumed > 0 {
            buf.drain(..consumed);
        }

        if frames.is_empty() {
            continue;
        }

        let mut st = state.lock().await;
        // Telemetry events accumulated while we hold a `q` borrow, emitted after
        // it is released (so `st.emit` doesn't alias `q`). Also a `torn` latch.
        let mut pending: Vec<Telemetry> = Vec::new();
        for frame in frames {
            let info = acp::inspect(&frame);

            // --- turn-close: a response carrying stopReason closes the turn
            // whose request id this matches (§9 / Phase 3). ---
            if info.stop_reason.is_some() {
                if let Some(id) = &info.id {
                    st.open_turns.remove(id);
                }
            }

            // --- demux by sessionId; connection-level frames (no sessionId, e.g.
            // the initialize response) go to a synthetic "-" session so they are
            // never dropped and still ordered. ---
            let key = info.session_id.clone().unwrap_or_else(|| "-".to_string());

            // All queue mutation + sample-building happens in this scope, which
            // ends (releasing the `q` borrow) before we touch `st.emit` / `st.torn`.
            let mut tear = false;
            {
                let q = st
                    .queues
                    .entry(key.clone())
                    .or_insert_with(SessionQueue::new);
                let was_empty = q.frames.is_empty();

                // Soft bound: stop *forwarding* this session and mark pressured,
                // but keep buffering (we never stop reading the agent). Surface it.
                if !q.pressured && q.frames.len() >= SOFT_BOUND_FRAMES {
                    q.pressured = true;
                    pending.push(Telemetry {
                        session: key.clone(),
                        queue_depth: q.frames.len(),
                        oldest_unread_ms: q.oldest_age_ms(),
                        lifecycle: "pressured".into(),
                    });
                    eprintln!(
                        "claude-pipe: agent '{agent_id}' session '{key}' hit soft bound \
                         ({SOFT_BOUND_FRAMES} frames) — pressured, surfacing on telemetry"
                    );
                }

                q.push(frame);

                // Hard memory bound: a single wedged session past the byte cap →
                // tear the lease (logged, surfaced). Never a mid-stream drop (§6.3).
                if q.bytes > HARD_BOUND_BYTES {
                    pending.push(Telemetry {
                        session: key.clone(),
                        queue_depth: q.frames.len(),
                        oldest_unread_ms: q.oldest_age_ms(),
                        lifecycle: "torn".into(),
                    });
                    eprintln!(
                        "claude-pipe: agent '{agent_id}' session '{key}' exceeded hard bound \
                         ({} bytes > {HARD_BOUND_BYTES}) — tearing lease (never-silent)",
                        q.bytes
                    );
                    // Drop this session's buffered contents (the whole stream is
                    // being torn cleanly, not a partial drop of a live one) and
                    // latch `torn` for the forwarder to observe and close the client.
                    q.frames.clear();
                    q.bytes = 0;
                    tear = true;
                } else if was_empty && !q.pressured {
                    // Newly non-empty + flowing — a cheap liveness sample.
                    pending.push(Telemetry {
                        session: key.clone(),
                        queue_depth: q.frames.len(),
                        oldest_unread_ms: q.oldest_age_ms(),
                        lifecycle: "flowing".into(),
                    });
                }
            }
            if tear {
                st.torn = true;
            }
        }
        for t in pending {
            st.emit(t);
        }
    }
}

/// **client→agent**: read raw bytes from the leased client and write them to the
/// agent's single stdin writer, byte-faithful. Tracks `session/prompt` requests
/// to open turns (the matching close is detected on the agent→client side).
async fn client_to_agent(
    agent_id: String,
    mut read_half: tokio::net::unix::OwnedReadHalf,
    agent_stdin: Arc<Mutex<ChildStdin>>,
    state: Arc<Mutex<RelayState>>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = match read_half.read(&mut chunk).await {
            Ok(0) => return, // client hung up
            Ok(n) => n,
            Err(_) => return,
        };
        buf.extend_from_slice(&chunk[..n]);

        // Frame the client's bytes so we can spot session/prompt opens — but
        // forward the ORIGINAL bytes (Invariant 1). We write each complete frame
        // through verbatim and keep any trailing partial for the next read.
        let (frames, consumed): (Vec<Vec<u8>>, usize) = {
            let (refs, consumed) = acp::split_frames(&buf);
            (refs.into_iter().map(|f| f.to_vec()).collect(), consumed)
        };

        if !frames.is_empty() {
            // Track turn opens before writing (so a steal racing the write still
            // sees the turn as open).
            {
                let mut st = state.lock().await;
                for frame in &frames {
                    let info = acp::inspect(frame);
                    if info.is_prompt_request {
                        if let Some(id) = info.id {
                            let sid = info.session_id.clone().unwrap_or_else(|| "-".into());
                            st.open_turns.insert(id, sid);
                        }
                    }
                }
            }
            // Write the framed bytes verbatim under the stdin lock.
            let mut w = agent_stdin.lock().await;
            for frame in &frames {
                if w.write_all(frame).await.is_err() {
                    eprintln!("claude-pipe: agent '{agent_id}' stdin write failed");
                    return;
                }
            }
            if w.flush().await.is_err() {
                return;
            }
            buf.drain(..consumed);
        }
    }
}

/// **queues→client**: fairly drain the per-session forward queues to the leased
/// client (round-robin, so no session starves another), honoring soft/hard
/// bounds. Exits when `stop` fires (steal/detach), the client hangs up, or the
/// hard bound tore the lease.
async fn queues_to_client(
    agent_id: String,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    state: Arc<Mutex<RelayState>>,
    mut stop: oneshot::Receiver<()>,
) {
    loop {
        // Honor a steal/detach immediately.
        if stop.try_recv().is_ok() {
            return;
        }

        // Pull one frame from the next session in round-robin order. We collect a
        // small batch under the lock, then release it before the (awaited) write,
        // so the demux task is never blocked by a slow client socket.
        let batch: Vec<Vec<u8>> = {
            let mut st = state.lock().await;
            if st.torn {
                eprintln!("claude-pipe: agent '{agent_id}' lease torn at hard bound — closing client");
                return;
            }
            drain_round_robin(&mut st, 64)
        };

        if batch.is_empty() {
            // Nothing to forward right now; yield briefly. (A notify could replace
            // this poll; the poll keeps the module simple and is cheap at idle.)
            tokio::time::sleep(Duration::from_millis(2)).await;
            continue;
        }

        for frame in batch {
            if write_half.write_all(&frame).await.is_err() {
                return; // client gone
            }
        }
        if write_half.flush().await.is_err() {
            return;
        }
    }
}

/// Drain up to `max` frames across sessions in round-robin order, skipping
/// **pressured** sessions (soft bound: we stopped forwarding them; §6.3). Returns
/// the frames in forward order. Advances the round-robin cursor fairly.
fn drain_round_robin(st: &mut RelayState, max: usize) -> Vec<Vec<u8>> {
    // Ensure every known session is in the rotation.
    let keys: Vec<String> = st.queues.keys().cloned().collect();
    for k in keys {
        if !st.drain_order.contains(&k) {
            st.drain_order.push_back(k);
        }
    }

    let mut out = Vec::new();
    let n_sessions = st.drain_order.len();
    if n_sessions == 0 {
        return out;
    }

    let mut checked = 0;
    while out.len() < max && checked < n_sessions {
        let Some(sid) = st.drain_order.pop_front() else { break };
        let mut progressed = false;
        if let Some(q) = st.queues.get_mut(&sid) {
            // Pressured sessions are NOT forwarded (soft bound) — the orchestrator
            // must drain them by valuing them; until then they stay buffered.
            if !q.pressured {
                if let Some(frame) = q.pop() {
                    out.push(frame);
                    progressed = true;
                }
            }
        }
        // Rotate this session to the back; keep going round until batch full or a
        // full lap with no progress.
        st.drain_order.push_back(sid);
        if progressed {
            checked = 0; // made progress; allow another full lap
        } else {
            checked += 1;
        }
    }
    out
}

