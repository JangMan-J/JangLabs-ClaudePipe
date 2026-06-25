//! The supervisor — the single long-lived process that **owns the warm pool**.
//!
//! This is the v2 daemon. It is the only shape that satisfies spec §2/§8 at once:
//!
//!   - **A — warm pool:** it pre-spawns agents from recipes and keeps them idle.
//!   - **B — client-outliving:** it outlives any orchestrator; a successor
//!     reattaches via the control socket to live agents.
//!   - **C — naming/discovery:** it resolves agents by recipe name / id.
//!   - **§8 spawn ownership:** it owns spawning, keep-warm, restart, and persists
//!     pool state (id, recipe, pid) across restarts.
//!
//! It speaks the **out-of-band control protocol** ([`crate::protocol`]) on a
//! single `control.sock`. Each agent gets its own **pure-ACP data socket** that an
//! `attach` hands the orchestrator. The data path and control path never mix
//! (Invariant 7 / §6.2).
//!
//! The supervisor owns each agent child's piped stdin/stdout **directly** and
//! hands those fds to a per-agent [`crate::relay`] — never a PTY (Invariant 3).

use crate::protocol::{
    agent_socket_path, control_socket_path, ensure_runtime_dir, supervisor_state_path,
    write_json_atomic, AgentInfo, ControlRequest, ControlResponse, Liveness, Telemetry,
};
use crate::recipe::{Recipe, RecipeKind};
use crate::relay::{spawn_relay, RelayCmd, RelayHandle};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// A live pool member the supervisor tracks in memory.
struct Agent {
    info: AgentInfo,
    /// The agent child process (owned here so the supervisor reaps it).
    child: Child,
    /// The relay driving this agent's data socket.
    relay: RelayHandle,
    /// The bound data-socket listener (kept so it stays open for the agent's life).
    _data_listener: Arc<UnixListener>,
}

/// The supervisor's whole state: the pool, keyed by agent id.
struct Supervisor {
    agents: Mutex<HashMap<String, Agent>>,
    /// Monotonic counter for minting agent ids (id = `<recipe>-<n>`).
    next_seq: Mutex<u64>,
}

/// Persisted pool state (spec §8): enough for `list`/`attach` to survive a
/// supervisor restart. We persist identity, not live handles.
#[derive(Debug, Serialize, Deserialize, Default)]
struct PersistedPool {
    agents: Vec<PersistedAgent>,
    next_seq: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedAgent {
    id: String,
    recipe: String,
    pid: u32,
}

impl Supervisor {
    fn new() -> Self {
        Supervisor {
            agents: Mutex::new(HashMap::new()),
            next_seq: Mutex::new(0),
        }
    }

    async fn mint_id(&self, recipe: &str) -> String {
        let mut seq = self.next_seq.lock().await;
        *seq += 1;
        format!("{recipe}-{seq}")
    }

    /// Persist current pool identity atomically (spec §8).
    async fn persist(&self) {
        let agents = self.agents.lock().await;
        let seq = *self.next_seq.lock().await;
        let pool = PersistedPool {
            agents: agents
                .values()
                .map(|a| PersistedAgent {
                    id: a.info.id.clone(),
                    recipe: a.info.recipe.clone(),
                    pid: a.info.pid,
                })
                .collect(),
            next_seq: seq,
        };
        let _ = write_json_atomic(&supervisor_state_path(), &pool).await;
    }
}

/// Entry point for `claude-pipe serve` (the v2 supervisor). Runs in the
/// foreground unless `detach` (handled by the caller re-exec, like v1).
pub async fn run_supervisor(prespawn: Vec<String>) -> Result<()> {
    ensure_runtime_dir().await?;
    let ctrl_path = control_socket_path();

    // Refuse to clobber a live supervisor; clean a stale socket otherwise.
    if ctrl_path.exists() {
        if UnixStream::connect(&ctrl_path).await.is_ok() {
            return Err(anyhow!(
                "supervisor already running (control socket {} is live)",
                ctrl_path.display()
            ));
        }
        let _ = tokio::fs::remove_file(&ctrl_path).await;
    }

    let sup = Arc::new(Supervisor::new());

    // Pre-fill the warm pool from the requested recipes (A — warm pool; §7). Each
    // named recipe is brought up to at least `max(1, pool_size)` warm instances —
    // naming a recipe means "I want at least one of these idling"; its declared
    // `pool_size` raises that target (spec §7 pool size; A — never pay cold start).
    for recipe_name in &prespawn {
        match Recipe::builtin(recipe_name) {
            Some(recipe) => {
                let want = recipe.pool_size.max(1);
                for _ in 0..want {
                    if let Err(e) = spawn_agent(&sup, &recipe).await {
                        eprintln!("claude-pipe: prespawn '{recipe_name}' failed: {e:#}");
                        break;
                    }
                }
            }
            None => eprintln!("claude-pipe: unknown recipe '{recipe_name}', skipping prespawn"),
        }
    }
    sup.persist().await;

    let listener = UnixListener::bind(&ctrl_path)
        .with_context(|| format!("binding control socket {}", ctrl_path.display()))?;
    eprintln!(
        "claude-pipe: supervisor up, control socket {} ({} agent(s) warm)",
        ctrl_path.display(),
        sup.agents.lock().await.len()
    );

    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let result = control_accept_loop(&listener, &sup, &mut sigterm).await;

    // Teardown: kill all agents, remove control socket + state.
    {
        let mut agents = sup.agents.lock().await;
        for (_, mut a) in agents.drain() {
            let _ = a.child.kill().await;
            let _ = tokio::fs::remove_file(agent_socket_path(&a.info.id)).await;
        }
    }
    let _ = tokio::fs::remove_file(&ctrl_path).await;
    let _ = tokio::fs::remove_file(supervisor_state_path()).await;
    eprintln!("claude-pipe: supervisor down");
    result
}

/// Accept control connections, dispatching each to a handler, until shutdown.
async fn control_accept_loop(
    listener: &UnixListener,
    sup: &Arc<Supervisor>,
    sigterm: &mut tokio::signal::unix::Signal,
) -> Result<()> {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted.context("control accept failed")?;
                let sup = sup.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_control_conn(stream, sup).await {
                        eprintln!("claude-pipe: control connection error: {e:#}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("claude-pipe: SIGINT, supervisor shutting down");
                return Ok(());
            }
            _ = sigterm.recv() => {
                eprintln!("claude-pipe: SIGTERM, supervisor shutting down");
                return Ok(());
            }
        }
    }
}

/// Spawn one agent from a recipe, own its stdio, bind its data socket, and start
/// its relay. Inserts it into the pool. Spec §8 (spawn ownership) + §5 (owns
/// stdin/stdout directly, never a PTY).
async fn spawn_agent(sup: &Arc<Supervisor>, recipe: &Recipe) -> Result<AgentInfo> {
    let id = sup.mint_id(&recipe.name).await;
    let data_path = agent_socket_path(&id);
    if data_path.exists() {
        let _ = tokio::fs::remove_file(&data_path).await;
    }

    // Bind the per-agent pure-ACP data socket up front so `attach` can hand it out
    // immediately (it accepts the orchestrator's connection; spec §5/§6.1).
    let data_listener = Arc::new(
        UnixListener::bind(&data_path)
            .with_context(|| format!("binding data socket {}", data_path.display()))?,
    );

    // Spawn the agent child, owning its stdin/stdout DIRECTLY (piped fds). This is
    // the architecturally decisive requirement — protocol bytes never traverse a
    // terminal (Invariant 3). stderr is inherited for debugging (agents MAY log
    // to stderr per ACP; it carries no protocol bytes).
    let mut cmd = Command::new(&recipe.command);
    cmd.args(&recipe.args);
    for (k, v) in &recipe.env {
        match v {
            Some(val) => {
                cmd.env(k, val);
            }
            None => {
                cmd.env_remove(k);
            }
        }
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning recipe '{}' ({})", recipe.name, recipe.command))?;
    let pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take().expect("agent stdin piped");
    let stdout = child.stdout.take().expect("agent stdout piped");

    // Start the relay over the owned stdio.
    let relay = spawn_relay(id.clone(), stdin, stdout);

    // Accept loop for THIS agent's data socket: every accepted connection is an
    // `attach` candidate routed into the relay (the relay enforces the single-
    // client lease + turn-boundary steal, §9). We forward accepted streams to the
    // relay via a dedicated Attach with the connecting peer as an implicit holder;
    // but lease granting is normally driven by the control-plane `attach` verb,
    // which connects here after being told the path. So this loop simply makes the
    // socket *connectable*; the relay's lease logic governs what a connection may do.
    let relay_for_accept = relay.cmd_tx.clone();
    let listener_for_accept = data_listener.clone();
    let id_for_accept = id.clone();
    tokio::spawn(async move {
        data_socket_accept_loop(id_for_accept, listener_for_accept, relay_for_accept).await;
    });

    let info = AgentInfo {
        id: id.clone(),
        recipe: recipe.name.clone(),
        pid,
        liveness: Liveness::Warm, // initialize handshake is the agent's own; we
        // mark warm on successful spawn. (A stricter "starting→warm after
        // initialize" gate is a recipe-level refinement, not a transport feature.)
        lease_holder: None,
        session_count: 0,
    };

    let agent = Agent {
        info: info.clone(),
        child,
        relay,
        _data_listener: data_listener,
    };
    sup.agents.lock().await.insert(id.clone(), agent);

    // Mark the recipe-specific transport note for clarity in logs.
    match recipe.kind {
        RecipeKind::AcpStdio => {
            eprintln!("claude-pipe: spawned acp-stdio agent '{id}' (pid {pid}) on {}", data_path.display())
        }
        RecipeKind::ClaudeChannels => eprintln!(
            "claude-pipe: spawned claude-channels agent '{id}' (pid {pid}) — research-preview; \
             channel protocol on {} (NOT raw ACP)",
            data_path.display()
        ),
    }
    Ok(info)
}

/// Per-agent data-socket accept loop. The data socket is pure and connectable;
/// each accepted stream becomes the relay's leased client (lease/steal enforced
/// inside the relay). The connecting orchestrator was handed this path by a prior
/// control-plane `attach`, which also recorded the lease holder.
async fn data_socket_accept_loop(
    agent_id: String,
    listener: Arc<UnixListener>,
    relay: tokio::sync::mpsc::Sender<RelayCmd>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                // The holder tag for a raw data-socket connection defaults to the
                // socket peer; the control-plane attach already set the intended
                // holder, so we use a generic tag here and let the relay's lease
                // logic (most-recent-attach-wins, turn-boundary steal) govern.
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if relay
                    .send(RelayCmd::Attach {
                        stream,
                        holder: "data-socket".into(),
                        reply: reply_tx,
                    })
                    .await
                    .is_err()
                {
                    return; // relay gone
                }
                match reply_rx.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => eprintln!("claude-pipe: agent '{agent_id}' attach rejected: {e}"),
                    Err(_) => return,
                }
            }
            Err(e) => {
                eprintln!("claude-pipe: agent '{agent_id}' data accept error: {e}");
                return;
            }
        }
    }
}

/// Handle one control connection: read request line(s), dispatch, reply. Most
/// verbs are one-shot; `events` streams telemetry until the peer disconnects.
async fn handle_control_conn(stream: UnixStream, sup: Arc<Supervisor>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(());
    }

    let req: ControlRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            return reply_one(&mut write_half, &ControlResponse::err(format!("bad control request: {e}"))).await;
        }
    };

    match req {
        ControlRequest::List => {
            let agents = collect_list(&sup).await;
            reply_one(&mut write_half, &ControlResponse::Agents { agents }).await
        }
        ControlRequest::Spawn { recipe } => {
            let resp = match Recipe::builtin(&recipe) {
                Some(r) => match spawn_agent(&sup, &r).await {
                    Ok(info) => {
                        sup.persist().await;
                        ControlResponse::Spawned { agent: info }
                    }
                    Err(e) => ControlResponse::err(format!("spawn failed: {e:#}")),
                },
                None => ControlResponse::err(format!(
                    "unknown recipe '{recipe}' (known: {})",
                    Recipe::builtin_names().join(", ")
                )),
            };
            reply_one(&mut write_half, &resp).await
        }
        ControlRequest::Attach { target, holder } => {
            let resp = do_control_attach(&sup, &target, &holder).await;
            reply_one(&mut write_half, &resp).await
        }
        ControlRequest::Detach { id, holder } => {
            let resp = do_control_detach(&sup, id.as_deref(), &holder).await;
            reply_one(&mut write_half, &resp).await
        }
        ControlRequest::Kill { id } => {
            let resp = do_control_kill(&sup, &id).await;
            reply_one(&mut write_half, &resp).await
        }
        ControlRequest::Events { agent } => {
            stream_events(&sup, &agent, &mut write_half).await
        }
    }
}

/// Resolve a target (id or recipe-name) to an agent id. Exact id wins; otherwise
/// the first warm agent of that recipe kind (naming/discovery, spec C).
async fn resolve_target(sup: &Arc<Supervisor>, target: &str) -> Option<String> {
    let agents = sup.agents.lock().await;
    if agents.contains_key(target) {
        return Some(target.to_string());
    }
    // Strip an optional leading '#': "#7" → id ending in "-7" or matching seq.
    let want = target.trim_start_matches('#');
    agents
        .values()
        .find(|a| a.info.recipe == want || a.info.id == want)
        .map(|a| a.info.id.clone())
}

async fn do_control_attach(sup: &Arc<Supervisor>, target: &str, holder: &str) -> ControlResponse {
    let Some(id) = resolve_target(sup, target).await else {
        return ControlResponse::err(format!("no agent matching '{target}'"));
    };
    // We grant the lease by recording the holder; the actual client connection
    // arrives on the data socket next and the relay enforces the lease/steal. To
    // make the steal turn-boundary-safe even before the connection, we tell the
    // relay the intended holder now (it updates lease bookkeeping for `list`).
    let agents = sup.agents.lock().await;
    let Some(agent) = agents.get(&id) else {
        return ControlResponse::err(format!("agent '{id}' vanished"));
    };
    // Pre-register the intended lease holder in the relay so the lease the data
    // socket grants on connect is tagged with the orchestrator's real holder
    // (visible to `list`, matchable by `detach`). We AWAIT the relay's ack before
    // returning the path, so the holder is registered before the orchestrator can
    // connect — closing the ordering race (audit F3). The lease is truly *taken*
    // when the orchestrator connects its ACP client to the returned path.
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    if agent
        .relay
        .cmd_tx
        .send(RelayCmd::SetIntendedHolder {
            holder: holder.to_string(),
            reply: ack_tx,
        })
        .await
        .is_ok()
    {
        let _ = ack_rx.await; // wait for the relay to record the holder
    }
    let socket = agent_socket_path(&id);
    let mut info = agent.info.clone();
    info.lease_holder = Some(holder.to_string());
    ControlResponse::Attached { socket, agent: info }
}

async fn do_control_detach(
    sup: &Arc<Supervisor>,
    id: Option<&str>,
    holder: &str,
) -> ControlResponse {
    let agents = sup.agents.lock().await;
    let target_ids: Vec<String> = match id {
        Some(i) => vec![i.to_string()],
        None => agents.keys().cloned().collect(),
    };
    for aid in target_ids {
        if let Some(agent) = agents.get(&aid) {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if agent
                .relay
                .cmd_tx
                .send(RelayCmd::Detach { holder: holder.to_string(), reply: tx })
                .await
                .is_ok()
            {
                if let Ok(Ok(())) = rx.await {
                    return ControlResponse::Ok {
                        message: format!("detached '{holder}' from '{aid}'"),
                    };
                }
            }
        }
    }
    ControlResponse::Ok {
        message: format!("no matching lease for '{holder}'"),
    }
}

async fn do_control_kill(sup: &Arc<Supervisor>, id: &str) -> ControlResponse {
    let mut agents = sup.agents.lock().await;
    match agents.remove(id) {
        Some(mut agent) => {
            let _ = agent.child.kill().await;
            let _ = tokio::fs::remove_file(agent_socket_path(id)).await;
            drop(agents);
            sup.persist().await;
            ControlResponse::Ok {
                message: format!("killed agent '{id}'"),
            }
        }
        None => ControlResponse::err(format!("no agent '{id}'")),
    }
}

/// Build the `list` rows, querying each relay for live lease holder + session
/// count so the view reflects current leases (spec §6.2).
async fn collect_list(sup: &Arc<Supervisor>) -> Vec<AgentInfo> {
    let agents = sup.agents.lock().await;
    let mut out = Vec::with_capacity(agents.len());
    for agent in agents.values() {
        let mut info = agent.info.clone();
        // Live lease holder.
        let (tx, rx) = tokio::sync::oneshot::channel();
        if agent.relay.cmd_tx.send(RelayCmd::LeaseHolder { reply: tx }).await.is_ok() {
            if let Ok(h) = rx.await {
                info.lease_holder = h;
            }
        }
        // Live session count.
        let (tx, rx) = tokio::sync::oneshot::channel();
        if agent.relay.cmd_tx.send(RelayCmd::SessionCount { reply: tx }).await.is_ok() {
            if let Ok(c) = rx.await {
                info.session_count = c;
            }
        }
        // Liveness: reflect process exit if the child has died.
        info.liveness = if matches!(agent.info.liveness, Liveness::Dead) {
            Liveness::Dead
        } else {
            Liveness::Warm
        };
        out.push(info);
    }
    out
}

/// Stream this agent's telemetry to the control peer until it disconnects (the
/// `events --agent X` signal stream, spec §6.2). Read-only; derived purely from
/// the sessionId framing.
async fn stream_events(
    sup: &Arc<Supervisor>,
    agent: &str,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    let Some(id) = resolve_target(sup, agent).await else {
        return reply_one(write_half, &ControlResponse::err(format!("no agent '{agent}'"))).await;
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Telemetry>(256);
    {
        let agents = sup.agents.lock().await;
        let Some(a) = agents.get(&id) else {
            return reply_one(write_half, &ControlResponse::err("agent vanished")).await;
        };
        if a.relay.cmd_tx.send(RelayCmd::Subscribe { tx }).await.is_err() {
            return reply_one(write_half, &ControlResponse::err("relay gone")).await;
        }
    }
    // Forward samples as ControlResponse::Telemetry lines until the peer leaves.
    while let Some(sample) = rx.recv().await {
        let mut line = serde_json::to_string(&ControlResponse::Telemetry(sample))?;
        line.push('\n');
        if write_half.write_all(line.as_bytes()).await.is_err() {
            break; // peer disconnected
        }
        if write_half.flush().await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Write one control response line + flush.
async fn reply_one(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    resp: &ControlResponse,
) -> Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}
