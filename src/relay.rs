//! The per-agent relay: a byte-faithful, semantics-blind, full-duplex ACP
//! conduit between a single leased client (the orchestrator) and one warm agent
//! child process.
//!
//! This is the load-bearing module — almost every spec invariant lives here:
//!
//!   - **Invariant 1 (byte-faithful):** both directions relay original bytes
//!     verbatim. We split a stream into newline-delimited frames only to read
//!     `sessionId`; we forward the *original frame bytes*, never a re-serialized
//!     form. Two independent, mutually non-blocking directions.
//!   - **Invariant 2 (semantics-blind):** the only parse is [`crate::acp`] —
//!     `sessionId` + the prompt/stopReason turn bracket + outstanding agent→client
//!     request ids (for steal safety). Nothing else. The `stopReason` *value* is
//!     read for telemetry only and is NEVER branched on (no method-semantics act).
//!   - **§6.3 fairness + OPEN-1 overflow:** demux agent→client frames by
//!     `sessionId` into per-session bounded forward queues; **continuously drain
//!     the agent's shared stdout always** (never halt the read); apply
//!     backpressure only on the forward side; at a session's **soft bound** stop
//!     forwarding that session and surface it "pressured" (continuously); at a
//!     **hard memory bound** tear the lease with a logged reason. Drop-oldest is
//!     forbidden.
//!   - **Invariant 5 (never-silent):** every lease teardown / drop / client
//!     disconnect is logged and surfaced on telemetry with a reason.
//!   - **§9 lease & steal:** single-client exclusive lease; an attach steals the
//!     lease, but only at a turn boundary — defined as *no `session/prompt` open
//!     AND no agent→client request awaiting a client response* (so a steal never
//!     orphans a server-initiated callback id). A dead agent unblocks the wait.
//!   - **Invariant 9 (strictly in-band):** this module opens **no** fd except the
//!     agent's stdio (owned by the supervisor) and the client socket handed to it.
//!
//! Concurrency shape (post-audit): the agent's stdin is written by a **single
//! dedicated writer task** fed by an mpsc channel — never a shared lock held
//! across an await — so a slow agent write can never block a steal or another
//! writer (audit F2). On steal/detach, **both** client directions are stopped
//! (audit F1). On agent death, the forwarder is allowed to **drain remaining
//! frames** before the client is closed (audit F9).

use crate::acp::{self, RpcId};
use crate::protocol::Telemetry;
use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
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

/// Upper bound on how long a handoff waits for the current turn's boundary before
/// refusing the steal. The spec says a handoff "MAY briefly wait" (§9); we keep
/// it bounded and short-ish, and a dead agent short-circuits the wait entirely
/// (audit F10). Not "briefly" in the sub-second sense, but bounded so a wedged
/// turn cannot block a handoff forever.
const STEAL_WAIT: Duration = Duration::from_secs(30);

/// How long an outstanding server-initiated callback id keeps BLOCKING a lease steal
/// before the steal-wait treats it as abandoned (the client will never answer). The
/// id is not dropped — a late response still clears it; this only relaxes the *wait*.
/// Deliberately LONGER than the channels facade's 30s `session/request_permission`
/// timeout (acp-facade.mjs) so a slow-but-real client answer always wins the race and
/// still blocks the steal; only a truly never-answered callback is aged out here.
const CALLBACK_STEAL_TTL: Duration = Duration::from_secs(45);

/// A command the supervisor sends to a running relay over its control channel.
pub enum RelayCmd {
    /// Hand a freshly-accepted client connection to this relay, taking the lease
    /// (a turn-boundary steal if one is held). `holder` is the lease-holder tag.
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
    /// leased under, so `list`/`detach` see the orchestrator's real tag — not the
    /// generic data-socket placeholder. Carries a `reply` the control-plane
    /// `attach` awaits BEFORE returning the path, closing the ordering race where
    /// the data connection could arrive before the holder was registered (F3).
    SetIntendedHolder {
        holder: String,
        reply: oneshot::Sender<()>,
    },
}

/// Shared, mutable relay state guarded by a single mutex. Kept small: the demux
/// bookkeeping and the turn tracker. The hot byte loops touch it only at frame
/// boundaries (cheap), never mid-frame and never across a socket-write await.
struct RelayState {
    /// Per-session forward queues (agent→client), drained fairly.
    queues: HashMap<String, SessionQueue>,
    /// Round-robin cursor over session ids, so no session starves another.
    drain_order: VecDeque<String>,
    /// Ids of in-flight client→agent `session/prompt` requests → their sessionId.
    /// A non-empty map means some turn is open (steal-unsafe). Closed when the
    /// matching `stopReason` response id arrives.
    open_turns: HashMap<RpcId, String>,
    /// Ids of outstanding **agent→client** requests (server-initiated: `fs/*`,
    /// `terminal/*`, `session/request_permission`) → (sessionId, registered-at).
    /// Tracked so a steal never proceeds while a callback id is unanswered, which
    /// would orphan it on the new client (§9 "no orphanable server-initiated
    /// request"; audit F4). Removed when the client's response with that id passes.
    ///
    /// The `Instant` bounds a pathology: a buggy or dead agent (e.g. the channels
    /// facade abandoning a `session/request_permission` whose client never answers,
    /// or any acp-stdio agent that drops a callback) would otherwise pin this map
    /// non-empty forever, making `steal_unsafe()` permanently true and refusing every
    /// future lease steal for the agent. The steal-WAIT (not `steal_unsafe()` itself)
    /// ignores a callback older than [`CALLBACK_STEAL_TTL`]; the entry stays in the
    /// map (a late response still cleans it normally — no orphan), it just stops
    /// blocking handoff once the client has provably had long enough to answer.
    open_callbacks: HashMap<RpcId, (String, Instant)>,
    /// Telemetry subscribers (the `events --agent` streams).
    telemetry: Vec<mpsc::Sender<Telemetry>>,
    /// Set when a session's hard memory bound was exceeded: the forwarder reads
    /// this and closes the client (lease torn cleanly; §6.3 hard bound).
    torn: bool,
    /// True once the agent's stdout has closed (process death). The forwarder
    /// drains remaining frames then exits; the steal-wait short-circuits.
    agent_dead: bool,
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
            open_callbacks: HashMap::new(),
            telemetry: Vec::new(),
            torn: false,
            agent_dead: false,
        }
    }

    /// True iff a steal is currently UNSAFE: a `session/prompt` turn is open, or a
    /// server-initiated callback id is awaiting the client's response. Either way,
    /// stealing now could orphan a JSON-RPC id (§9). This is agent-wide, not
    /// per-session: `attach` carries no session hint, so the conservative correct
    /// choice is to wait until the agent has no outstanding ids at all.
    ///
    /// Retained as the STRICT predicate that pins the §9 invariant (and is exercised
    /// by `steal_unsafe_tracks_turns_and_callbacks`). The steal-WAIT uses the TTL-aware
    /// [`steal_unsafe_excluding_expired`] instead, so a never-answered callback cannot
    /// pin the agent forever; `steal_unsafe` stays the un-aged ground truth.
    #[cfg_attr(not(test), allow(dead_code))]
    fn steal_unsafe(&self) -> bool {
        !self.open_turns.is_empty() || !self.open_callbacks.is_empty()
    }

    /// Like [`steal_unsafe`], but a callback older than `ttl` no longer counts as
    /// blocking — used only by the steal-WAIT so a never-answered callback cannot
    /// pin the agent un-stealable forever (the wedge fixed here). Open turns always
    /// block regardless of age (a turn closes on its own stopReason, not by aging).
    /// The expired entry is intentionally NOT removed: if a late response ever
    /// arrives it still clears the id normally via `client_to_agent`, so no id is
    /// orphaned — this only stops the *handoff* from waiting on a dead callback.
    fn steal_unsafe_excluding_expired(&self, ttl: Duration) -> bool {
        if !self.open_turns.is_empty() {
            return true;
        }
        self.open_callbacks
            .values()
            .any(|(_, registered_at)| registered_at.elapsed() < ttl)
    }

    /// Emit a telemetry sample to all subscribers (best-effort; a closed
    /// subscriber is dropped). Never-silent: pressure/tear/disconnect flow here.
    fn emit(&mut self, t: Telemetry) {
        self.telemetry.retain(|tx| match tx.try_send(t.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => true, // keep; slow subscriber
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }
}

/// A running relay handle the supervisor keeps. Owns the command channel.
pub struct RelayHandle {
    pub cmd_tx: mpsc::Sender<RelayCmd>,
}

/// Spawn the relay for one agent. Takes ownership of the agent child's piped
/// stdin/stdout. Returns a handle the supervisor uses to route attaches/
/// detaches/telemetry. Runs until the agent's stdout closes or it is dropped.
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
    // Fires whenever a turn/callback closes — lets the steal-wait wake without
    // polling (audit F10 responsiveness; also fired on agent death).
    let boundary = Arc::new(Notify::new());

    // Single dedicated agent-stdin writer task (audit F2): all client→agent frames
    // are sent through this mpsc, so no client task holds a lock across an awaited
    // write. A slow agent stdin backs up THIS channel only; it never blocks a
    // steal, a detach, or the demux. The writer owns the ChildStdin exclusively.
    let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(256);
    let stdin_id = agent_id.clone();
    tokio::spawn(async move {
        agent_stdin_writer(stdin_id, agent_stdin, stdin_rx).await;
    });

    // Continuously-draining agent→client demux task. NEVER stops reading the
    // agent's shared stdout (OPEN-1 rule 1) — it only buffers onto per-session
    // queues. Tracks turn close + callback open/close as frames pass.
    let demux_state = state.clone();
    let demux_boundary = boundary.clone();
    let demux_id = agent_id.clone();
    let demux = tokio::spawn(async move {
        agent_to_queues(demux_id, agent_stdout, demux_state, demux_boundary).await;
    });

    // The current lease (the active client's two stop switches + holder tag).
    let mut lease: Option<Lease> = None;
    // Holder a forthcoming raw data-socket connection adopts (set synchronously
    // by the control-plane attach before it returns the path; F3).
    let mut intended_holder: Option<String> = None;

    loop {
        tokio::select! {
            // Agent stdout closed → mark dead, wake any steal-wait, drain + finish.
            _ = wait_demux_done(&demux) => {
                let mut st = state.lock().await;
                st.agent_dead = true;
                st.emit(Telemetry { session: "-".into(), queue_depth: 0, oldest_unread_ms: 0, lifecycle: "agent_dead".into() });
                drop(st);
                boundary.notify_waiters();
                // Give the active forwarder a moment to drain remaining frames to
                // the client before we tear everything down (audit F9).
                if let Some(l) = &lease {
                    l.wait_drained().await;
                }
                break;
            }
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    RelayCmd::Attach { stream, holder, reply } => {
                        let effective = if holder == "data-socket" {
                            intended_holder.take().unwrap_or(holder)
                        } else {
                            holder
                        };
                        let r = do_attach(&agent_id, &state, &stdin_tx, &boundary, &mut lease, stream, effective).await;
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
                    RelayCmd::SetIntendedHolder { holder, reply } => {
                        intended_holder = Some(holder);
                        let _ = reply.send(()); // ack so the path isn't returned early (F3)
                    }
                }
            }
        }
    }

    demux.abort();
    Ok(())
}

/// An active lease: the holder tag, kill switches for BOTH client directions, and
/// a drained-signal the relay awaits on agent death so buffered frames flush.
struct Lease {
    holder: String,
    /// Stops the client→agent reader task (audit F1: this MUST be stopped on
    /// steal/detach, not just the forwarder).
    stop_reader: Option<oneshot::Sender<()>>,
    /// Stops the agent→client forwarder task.
    stop_forwarder: Option<oneshot::Sender<()>>,
    /// Set by the forwarder when it has finished draining (used on agent death).
    drained: Arc<Notify>,
}

impl Lease {
    /// Tear down both client directions (steal/detach). Idempotent.
    fn stop(&mut self) {
        if let Some(s) = self.stop_reader.take() {
            let _ = s.send(());
        }
        if let Some(s) = self.stop_forwarder.take() {
            let _ = s.send(());
        }
    }
    /// Wait (bounded) for the forwarder to signal it has drained, on agent death.
    async fn wait_drained(&self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.drained.notified()).await;
    }
}

/// Grant the lease to a new client, stealing at a turn boundary if needed (§9).
async fn do_attach(
    agent_id: &str,
    state: &Arc<Mutex<RelayState>>,
    stdin_tx: &mpsc::Sender<Vec<u8>>,
    boundary: &Arc<Notify>,
    lease: &mut Option<Lease>,
    stream: UnixStream,
    holder: String,
) -> Result<(), String> {
    // Steal safety: permitted only when the agent has no open turn AND no
    // outstanding callback id (§9; audit F4). If unsafe, wait (bounded) for a
    // boundary; a dead agent unblocks immediately.
    if lease.is_some() {
        if !wait_turn_boundary(state, boundary).await {
            return Err(
                "steal refused: agent stayed mid-turn/awaiting-callback past the handoff wait window"
                    .into(),
            );
        }
        if let Some(mut old) = lease.take() {
            old.stop(); // F1: stop BOTH directions of the old client
        }
    }

    let (read_half, write_half) = stream.into_split();
    let (stop_reader_tx, stop_reader_rx) = oneshot::channel();
    let (stop_fwd_tx, stop_fwd_rx) = oneshot::channel();
    let drained = Arc::new(Notify::new());

    // client→agent reader: frame the client's bytes, track prompt-opens +
    // callback-closes, and hand frames to the dedicated stdin writer. Holds no
    // lock across an await onto the agent.
    let c2a_state = state.clone();
    let c2a_boundary = boundary.clone();
    let c2a_stdin = stdin_tx.clone();
    let c2a_id = agent_id.to_string();
    tokio::spawn(async move {
        client_to_agent(c2a_id, read_half, c2a_stdin, c2a_state, c2a_boundary, stop_reader_rx).await;
    });

    // agent→client forwarder: fairly drain per-session queues to this client,
    // honoring soft/hard bounds, and drain-on-death.
    let a2c_state = state.clone();
    let a2c_id = agent_id.to_string();
    let a2c_drained = drained.clone();
    tokio::spawn(async move {
        queues_to_client(a2c_id, write_half, a2c_state, stop_fwd_rx, a2c_drained).await;
    });

    *lease = Some(Lease {
        holder,
        stop_reader: Some(stop_reader_tx),
        stop_forwarder: Some(stop_fwd_tx),
        drained,
    });
    Ok(())
}

fn do_detach(lease: &mut Option<Lease>, holder: &str) -> Result<(), String> {
    match lease {
        Some(l) if l.holder == holder => {
            if let Some(mut old) = lease.take() {
                old.stop(); // F1: both directions
            }
            Ok(())
        }
        Some(l) => Err(format!("lease held by '{}', not '{holder}'", l.holder)),
        None => Err("no lease to detach".into()),
    }
}

/// Wait until a steal is safe (no open turn, no outstanding callback), bounded by
/// [`STEAL_WAIT`]; a dead agent short-circuits to safe. Notify-driven (no poll).
async fn wait_turn_boundary(state: &Arc<Mutex<RelayState>>, boundary: &Arc<Notify>) -> bool {
    let deadline = Instant::now() + STEAL_WAIT;
    loop {
        {
            let st = state.lock().await;
            // Use the TTL-aware check: an open turn (or a still-fresh callback) blocks,
            // but a callback the client has had >CALLBACK_STEAL_TTL to answer and never
            // did no longer pins the agent un-stealable (the never-answered-callback
            // wedge). A dead agent short-circuits to safe regardless.
            if st.agent_dead || !st.steal_unsafe_excluding_expired(CALLBACK_STEAL_TTL) {
                return true;
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        // Wake on the next boundary event or the deadline, whichever first.
        let _ = tokio::time::timeout(remaining, boundary.notified()).await;
    }
}

/// Block until the demux task has finished (agent stdout closed).
async fn wait_demux_done(demux: &tokio::task::JoinHandle<()>) {
    while !demux.is_finished() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The single agent-stdin writer (audit F2). Receives already-framed byte buffers
/// and writes them verbatim to the agent's stdin, one mpsc message at a time. No
/// other task touches the agent's stdin, so writes never interleave and no lock is
/// ever held across the awaited write. A slow agent backs up this channel only.
async fn agent_stdin_writer(agent_id: String, mut stdin: ChildStdin, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(bytes) = rx.recv().await {
        if stdin.write_all(&bytes).await.is_err() {
            eprintln!("claude-pipe: agent '{agent_id}' stdin write failed (agent gone?)");
            return;
        }
        if stdin.flush().await.is_err() {
            return;
        }
    }
}

/// **Agent→queues**: continuously read the agent's shared stdout, split into
/// frames, demux by `sessionId`, push onto per-session forward queues, and track
/// turn close + callback open. NEVER halts the read (OPEN-1 rule 1). Returns when
/// stdout closes (agent death).
async fn agent_to_queues(
    agent_id: String,
    mut stdout: ChildStdout,
    state: Arc<Mutex<RelayState>>,
    boundary: Arc<Notify>,
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
        let mut pending: Vec<Telemetry> = Vec::new();
        let mut closed_something = false;
        for frame in frames {
            let info = acp::inspect(&frame);

            // --- Resolve the routing key, preserving per-session FIFO ORDER. ---
            // A frame's own `params.sessionId`/`result.sessionId` wins. But a
            // *response* (id, no method, no sessionId field) — e.g. the prompt's
            // own `stopReason`, or a session/new ack — must be ordered behind the
            // frames of the session it belongs to, NOT shunted to the synthetic
            // "-" lane where it could overtake that session's still-queued chunks.
            // We recover its session from the id→session maps we already keep
            // (open_turns for prompt responses; open_callbacks for the agent's own
            // request ids, though those are agent→client requests not responses).
            // This is the fix for the stopReason-overtakes-chunks reordering bug.
            let key = if let Some(sid) = &info.session_id {
                sid.clone()
            } else if let Some(id) = &info.id {
                st.open_turns
                    .get(id)
                    .or_else(|| st.open_callbacks.get(id).map(|(sid, _)| sid))
                    .cloned()
                    .unwrap_or_else(|| "-".to_string())
            } else {
                "-".to_string()
            };

            // turn-close: a response carrying stopReason closes the prompt turn
            // whose id this matches (§9 / Phase 3). Done AFTER key resolution so the
            // stopReason frame is still routed into its session's queue (above).
            if info.stop_reason.is_some() {
                if let Some(id) = &info.id {
                    if st.open_turns.remove(id).is_some() {
                        closed_something = true;
                    }
                }
            }
            // callback-open: an agent→client REQUEST (has both a method and an id)
            // is server-initiated (fs/*, terminal/*, request_permission). Record
            // its id as outstanding until the client answers it (audit F4). A
            // session/prompt is client→agent, so it never appears on this stream;
            // anything here with method+id is a callback.
            if info.has_method && info.id.is_some() {
                if let Some(id) = &info.id {
                    let sid = info.session_id.clone().unwrap_or_else(|| "-".into());
                    st.open_callbacks.insert(id.clone(), (sid, Instant::now()));
                }
            }

            let mut tear = false;
            {
                let q = st.queues.entry(key.clone()).or_insert_with(SessionQueue::new);
                let was_empty = q.frames.is_empty();
                q.push(frame); // push first, THEN evaluate bounds (audit F6)

                if !q.pressured && q.frames.len() >= SOFT_BOUND_FRAMES {
                    // Soft bound just crossed: stop forwarding this session, mark
                    // pressured, surface it.
                    q.pressured = true;
                    pending.push(sample(&key, q, "pressured"));
                    eprintln!(
                        "claude-pipe: agent '{agent_id}' session '{key}' hit soft bound \
                         ({SOFT_BOUND_FRAMES} frames) — pressured, surfacing on telemetry"
                    );
                } else if q.pressured {
                    // Already pressured and still growing — surface CONTINUOUSLY so
                    // the orchestrator sees depth pegged + oldest_unread_ms climbing
                    // (audit F7; §6.3 "loudly surfaced").
                    pending.push(sample(&key, q, "pressured"));
                } else if was_empty {
                    // Newly non-empty + flowing — a cheap liveness sample.
                    pending.push(sample(&key, q, "flowing"));
                }

                if q.bytes > HARD_BOUND_BYTES {
                    // Hard bound: capture depth+age BEFORE clearing so telemetry
                    // reports what was lost (audit F8; never-silent), then tear.
                    let mut s = sample(&key, q, "torn");
                    s.lifecycle = "torn".into();
                    pending.push(s);
                    eprintln!(
                        "claude-pipe: agent '{agent_id}' session '{key}' exceeded hard bound \
                         ({} bytes > {HARD_BOUND_BYTES}) — tearing lease (never-silent)",
                        q.bytes
                    );
                    q.frames.clear();
                    q.bytes = 0;
                    tear = true;
                }
            }
            if tear {
                st.torn = true;
            }
        }
        for t in pending {
            st.emit(t);
        }
        drop(st);
        if closed_something {
            boundary.notify_waiters(); // a turn closed → a waiting steal may proceed
        }
    }
}

/// Build a telemetry sample for `session` reflecting its current queue state.
fn sample(session: &str, q: &SessionQueue, lifecycle: &str) -> Telemetry {
    Telemetry {
        session: session.to_string(),
        queue_depth: q.frames.len(),
        oldest_unread_ms: q.oldest_age_ms(),
        lifecycle: lifecycle.to_string(),
    }
}

/// **client→agent**: read raw bytes from the leased client, frame them (to spot
/// `session/prompt` opens and callback-response closes), and hand the ORIGINAL
/// bytes to the dedicated stdin writer (Invariant 1). Stops on `stop` (steal/
/// detach; audit F1) or client hangup.
async fn client_to_agent(
    agent_id: String,
    mut read_half: tokio::net::unix::OwnedReadHalf,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    state: Arc<Mutex<RelayState>>,
    boundary: Arc<Notify>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = tokio::select! {
            _ = &mut stop => return, // steal/detach: stop this direction at once (F1)
            r = read_half.read(&mut chunk) => match r {
                Ok(0) => return,     // client hung up
                Ok(n) => n,
                Err(_) => return,
            }
        };
        buf.extend_from_slice(&chunk[..n]);

        let (frames, consumed): (Vec<Vec<u8>>, usize) = {
            let (refs, consumed) = acp::split_frames(&buf);
            (refs.into_iter().map(|f| f.to_vec()).collect(), consumed)
        };
        if frames.is_empty() {
            continue;
        }

        // Track turn-opens (prompt requests) and callback-closes (client responses
        // to a server-initiated id) BEFORE writing, so a racing steal sees the
        // turn/callback as open.
        let mut closed_callback = false;
        {
            let mut st = state.lock().await;
            for frame in &frames {
                let info = acp::inspect(frame);
                if info.is_prompt_request {
                    if let Some(id) = info.id.clone() {
                        let sid = info.session_id.clone().unwrap_or_else(|| "-".into());
                        st.open_turns.insert(id, sid);
                    }
                }
                // A client→agent RESPONSE (has an id but NO method) answering a
                // server-initiated request closes that callback id (audit F4).
                if !info.has_method && info.id.is_some() {
                    if let Some(id) = &info.id {
                        if st.open_callbacks.remove(id).is_some() {
                            closed_callback = true;
                        }
                    }
                }
            }
        }
        if closed_callback {
            boundary.notify_waiters();
        }

        // Forward the framed bytes verbatim via the dedicated writer. If the
        // writer is gone (agent dead), stop.
        for frame in frames {
            if stdin_tx.send(frame).await.is_err() {
                eprintln!("claude-pipe: agent '{agent_id}' stdin writer gone; closing client→agent");
                return;
            }
        }
        buf.drain(..consumed);
    }
}

/// **queues→client**: fairly drain the per-session forward queues to the leased
/// client (round-robin), honoring soft/hard bounds. Exits when `stop` fires
/// (steal/detach), the client hangs up, or the hard bound tore the lease. On
/// agent death, drains whatever remains, then signals `drained` (audit F9).
async fn queues_to_client(
    agent_id: String,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    state: Arc<Mutex<RelayState>>,
    mut stop: oneshot::Receiver<()>,
    drained: Arc<Notify>,
) {
    loop {
        if stop.try_recv().is_ok() {
            return;
        }

        let (batch, dead) = {
            let mut st = state.lock().await;
            if st.torn {
                eprintln!("claude-pipe: agent '{agent_id}' lease torn at hard bound — closing client");
                return;
            }
            let batch = drain_round_robin(&mut st, 64);
            (batch, st.agent_dead)
        };

        if batch.is_empty() {
            if dead {
                // Agent is gone and nothing left to forward — signal drained, exit.
                drained.notify_waiters();
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
            continue;
        }

        for frame in batch {
            if let Err(e) = write_half.write_all(&frame).await {
                eprintln!("claude-pipe: agent '{agent_id}' write to client failed: {e} (client gone)");
                return; // never-silent: logged (audit F11)
            }
        }
        if write_half.flush().await.is_err() {
            eprintln!("claude-pipe: agent '{agent_id}' flush to client failed (client gone)");
            return;
        }
    }
}

/// Drain up to `max` frames across sessions in round-robin order, skipping
/// **pressured** sessions (soft bound; §6.3). Fairness fix (audit F5): a session
/// is counted toward the "no-progress lap" budget only once per lap and only when
/// it was actually *eligible* (not pressured) but empty — pressured sessions are
/// skipped without consuming the budget, so they cannot prematurely end the lap
/// and starve eligible sessions behind them.
fn drain_round_robin(st: &mut RelayState, max: usize) -> Vec<Vec<u8>> {
    let keys: Vec<String> = st.queues.keys().cloned().collect();
    for k in keys {
        if !st.drain_order.contains(&k) {
            st.drain_order.push_back(k);
        }
    }

    let mut out = Vec::new();
    let n = st.drain_order.len();
    if n == 0 {
        return out;
    }

    // Terminate when the batch is full OR a *complete lap* of all n sessions makes
    // zero progress (every session was either pressured-skipped or eligible-empty).
    // We process in windows of n visits (one lap); if a lap pops nothing, no further
    // lap can either (queues only shrink here), so we stop. This drains all
    // available frames from eligible sessions, round-robin fair, and never spins on
    // an all-pressured set (audit F5).
    loop {
        let mut popped_this_lap = 0usize;
        for _ in 0..n {
            if out.len() >= max {
                return out;
            }
            let Some(sid) = st.drain_order.pop_front() else { break };
            if let Some(q) = st.queues.get_mut(&sid) {
                if !q.pressured {
                    if let Some(frame) = q.pop() {
                        out.push(frame);
                        popped_this_lap += 1;
                    }
                }
            }
            st.drain_order.push_back(sid);
        }
        if popped_this_lap == 0 {
            return out; // a full lap with no progress → done
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st_with(sessions: &[(&str, usize, bool)]) -> RelayState {
        let mut st = RelayState::new();
        for (name, count, pressured) in sessions {
            let mut q = SessionQueue::new();
            for i in 0..*count {
                q.push(format!("{name}-{i}\n").into_bytes());
            }
            q.pressured = *pressured;
            st.queues.insert((*name).to_string(), q);
        }
        st
    }

    #[test]
    fn round_robin_is_fair_across_sessions() {
        // Two flowing sessions, 3 frames each; a batch of 6 must take from both.
        let mut st = st_with(&[("a", 3, false), ("b", 3, false)]);
        let out = drain_round_robin(&mut st, 6);
        assert_eq!(out.len(), 6);
        let from_a = out.iter().filter(|f| f.starts_with(b"a-")).count();
        let from_b = out.iter().filter(|f| f.starts_with(b"b-")).count();
        assert_eq!(from_a, 3);
        assert_eq!(from_b, 3);
    }

    #[test]
    fn round_robin_skips_pressured_but_drains_eligible() {
        // 'a' pressured (must NOT be forwarded), 'b' flowing — only 'b' drains, and
        // the pressured 'a' at the front must not starve 'b' (audit F5).
        let mut st = st_with(&[("a", 100, true), ("b", 2, false)]);
        let out = drain_round_robin(&mut st, 64);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|f| f.starts_with(b"b-")));
        // 'a' still holds all its frames (untouched).
        assert_eq!(st.queues.get("a").unwrap().frames.len(), 100);
    }

    #[test]
    fn round_robin_empty_when_all_pressured() {
        let mut st = st_with(&[("a", 10, true), ("b", 10, true)]);
        let out = drain_round_robin(&mut st, 64);
        assert!(out.is_empty());
    }

    #[test]
    fn steal_unsafe_tracks_turns_and_callbacks() {
        let mut st = RelayState::new();
        assert!(!st.steal_unsafe());
        st.open_turns.insert(RpcId::Num(1), "s".into());
        assert!(st.steal_unsafe());
        st.open_turns.clear();
        assert!(!st.steal_unsafe());
        // A pending callback alone also makes a steal unsafe (F4).
        st.open_callbacks
            .insert(RpcId::Num(99), ("s".into(), Instant::now()));
        assert!(st.steal_unsafe());
    }

    #[test]
    fn steal_wait_ages_out_a_never_answered_callback() {
        // The wedge fix: a callback older than CALLBACK_STEAL_TTL no longer blocks the
        // steal-WAIT, while a fresh one (and any open turn) still does. The strict
        // steal_unsafe() is unchanged — only the TTL-aware variant relaxes.
        let mut st = RelayState::new();
        let ttl = Duration::from_millis(50);

        // A fresh callback blocks both predicates.
        st.open_callbacks
            .insert(RpcId::Num(1), ("s".into(), Instant::now()));
        assert!(st.steal_unsafe());
        assert!(st.steal_unsafe_excluding_expired(ttl));

        // An aged callback: strict still blocks, but the TTL-aware wait does not.
        let old = Instant::now() - Duration::from_millis(100);
        st.open_callbacks.insert(RpcId::Num(1), ("s".into(), old));
        assert!(
            st.steal_unsafe(),
            "strict predicate still counts the id (no orphan)"
        );
        assert!(
            !st.steal_unsafe_excluding_expired(ttl),
            "aged-out callback must not block the steal-wait"
        );
        // The id is still tracked, so a late response can still clear it normally.
        assert!(st.open_callbacks.contains_key(&RpcId::Num(1)));

        // An open turn always blocks regardless of callback age.
        st.open_turns.insert(RpcId::Num(2), "s".into());
        assert!(st.steal_unsafe_excluding_expired(ttl));
    }
}
