//! The daemon: owns one persistent `claude` process and serves request/reply
//! over a Unix-domain socket.
//!
//! Concurrency model: Claude's streaming stdin/stdout is a *single ordered
//! channel* — turn N must fully complete (its `result` event seen) before turn
//! N+1's message is written, or replies would interleave. So the daemon
//! serializes: every socket connection hands its request to a single worker
//! task via an mpsc queue, and the worker drives the claude child one turn at a
//! time. This is exactly right for dictation (turns are inherently sequential)
//! and keeps the design simple and correct.

use crate::protocol::{
    ensure_runtime_dir, socket_path, state_path, Request, Response, State,
};
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{timeout, Duration, Instant};

/// One unit of work for the serializing worker: a request plus a channel to
/// return the response on.
struct Job {
    req: Request,
    reply_tx: oneshot::Sender<Response>,
}

/// Entry point for `claude-pipe up`.
pub async fn run_up(
    session: String,
    model: Option<String>,
    system: Option<String>,
    resume: Option<String>,
    full: bool,
    detach: bool,
) -> Result<()> {
    if detach {
        // Re-exec ourselves without --detach, fully detached from this shell,
        // and return once the socket appears. Keeps `up --detach` ergonomic
        // for hooks/launchers without us owning a daemonize dependency.
        return spawn_detached(&session, &model, &system, &resume, full).await;
    }

    ensure_runtime_dir().await?;
    let sock = socket_path(&session);

    // Refuse to clobber a live daemon; clean up a stale socket otherwise.
    if sock.exists() {
        if UnixStream::connect(&sock).await.is_ok() {
            return Err(anyhow!(
                "session '{session}' already running (socket {} is live)",
                sock.display()
            ));
        }
        let _ = tokio::fs::remove_file(&sock).await;
    }

    // Spawn the persistent claude child.
    let mut child = spawn_claude(&model, &system, &resume, &full)
        .context("spawning persistent claude process")?;
    let stdin = child.stdin.take().expect("claude stdin piped");
    let stdout = child.stdout.take().expect("claude stdout piped");

    // Write initial state (session_id filled in after the first turn).
    write_state(
        &session,
        &State {
            pid: std::process::id(),
            session_id: None,
            model: model.clone(),
        },
    )
    .await?;

    // The serializing worker owns the child's stdin/stdout.
    let (job_tx, job_rx) = mpsc::channel::<Job>(64);
    let session_for_worker = session.clone();
    let worker = tokio::spawn(async move {
        worker_loop(stdin, stdout, job_rx, session_for_worker).await
    });

    // Listen for clients.
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("binding socket {}", sock.display()))?;
    eprintln!(
        "claude-pipe: session '{session}' up, listening on {}",
        sock.display()
    );

    // Accept loop, with clean shutdown on SIGINT/SIGTERM and on child exit.
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )?;
    let result = run_accept_loop(&listener, &job_tx, &mut child, &mut sigterm).await;

    // Teardown: drop the queue (ends the worker), kill claude, remove socket.
    drop(job_tx);
    let _ = worker.await;
    let _ = child.kill().await;
    let _ = tokio::fs::remove_file(&sock).await;
    let _ = tokio::fs::remove_file(state_path(&session)).await;
    eprintln!("claude-pipe: session '{session}' down");
    result
}

/// Accept connections and forward each request to the worker, until a shutdown
/// signal or the claude child exits.
async fn run_accept_loop(
    listener: &UnixListener,
    job_tx: &mpsc::Sender<Job>,
    child: &mut Child,
    sigterm: &mut tokio::signal::unix::Signal,
) -> Result<()> {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted.context("accept failed")?;
                let tx = job_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, tx).await {
                        eprintln!("claude-pipe: connection error: {e:#}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("claude-pipe: SIGINT, shutting down");
                return Ok(());
            }
            _ = sigterm.recv() => {
                eprintln!("claude-pipe: SIGTERM, shutting down");
                return Ok(());
            }
            status = child.wait() => {
                return Err(anyhow!("claude process exited unexpectedly: {status:?}"));
            }
        }
    }
}

/// Default system prompt used in lean mode when the consumer gives none. Kept
/// tiny on purpose — it replaces Claude's full coding-agent prompt, which is the
/// single biggest per-turn cache reduction.
const DEFAULT_LEAN_SYSTEM: &str =
    "You are a fast assistant invoked over a pipe. Answer directly and concisely. \
     Output only the answer — no preamble, no markdown fences, no commentary.";

/// Spawn `claude` in persistent streaming-JSON mode. The flag combo is exactly
/// what claude 2.1.x requires for a long-lived multi-turn process.
///
/// `full = false` (the default) strips per-turn overhead: no tools, no MCP, no
/// settings/hooks, replaced system prompt — see the body. `full = true` gives
/// Claude's normal agent loadout for consumers that need it.
fn spawn_claude(
    model: &Option<String>,
    system: &Option<String>,
    resume: &Option<String>,
    full: &bool,
) -> Result<Child> {
    let mut cmd = Command::new("claude");
    cmd.arg("-p")
        .arg("--verbose") // required by --output-format=stream-json under -p
        .arg("--input-format")
        .arg("stream-json")
        .arg("--output-format")
        .arg("stream-json");
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }

    if *full {
        // Full mode: the consumer wants Claude's normal agent loadout (tools,
        // MCP, settings/hooks). Used for "voice-driven agent" style consumers.
        // System prompt is APPENDED to Claude's default so the agent prompt
        // stays intact.
        if let Some(s) = system {
            cmd.arg("--append-system-prompt").arg(s);
        }
    } else {
        // LEAN MODE (default): strip every per-turn overhead source so each turn
        // carries no tool/MCP/hook/cache baggage. Measured effect: cache
        // creation/read tokens drop from ~8.7k/~17.7k to 0, the SessionStart
        // hook stops firing, and no MCP server loads. Right for a router/cleanup
        // consumer (the common case) that only transforms text and never acts.
        //   --system-prompt          : REPLACE the default coding-agent prompt
        //                              (the single biggest cache reduction).
        //   --tools ""               : no built-in tools (a router never acts).
        //   --strict-mcp-config      : load no MCP servers (no --mcp-config given).
        //   --setting-sources ""     : load no user/project/local settings, so
        //                              SessionStart hooks + project memory don't
        //                              attach to every turn.
        //   --exclude-dynamic-...    : move cwd/env/git out of the cached prompt
        //                              for better cache reuse.
        // NOTE: we do NOT pass --no-session-persistence; it would break --resume
        // (our restart-recovery story), and session save is cheap.
        let prompt = system.clone().unwrap_or_else(|| DEFAULT_LEAN_SYSTEM.to_string());
        cmd.arg("--system-prompt")
            .arg(prompt)
            .arg("--tools")
            .arg("")
            .arg("--strict-mcp-config")
            .arg("--setting-sources")
            .arg("")
            .arg("--exclude-dynamic-system-prompt-sections")
            .arg("--permission-mode")
            .arg("default");
    }

    if let Some(r) = resume {
        cmd.arg("--resume").arg(r);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Let claude's stderr pass through to the daemon's stderr for debugging.
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    cmd.spawn().context("failed to exec `claude` (is it on PATH?)")
}

/// The single worker that serializes turns against the claude child.
async fn worker_loop(
    stdin: ChildStdin,
    stdout: ChildStdout,
    mut job_rx: mpsc::Receiver<Job>,
    session: String,
) {
    let mut stdin = stdin;
    let mut reader = BufReader::new(stdout);
    let mut session_id: Option<String> = None;
    // True when a previous turn TIMED OUT: claude is still producing that turn's
    // output, so its `result` is still pending on stdout. We must consume it
    // before the next turn, or every subsequent reply would be off-by-one
    // (returning the prior turn's late result). This is the streaming-protocol
    // desync hazard, fixed by draining before serving the next request.
    let mut owes_result = false;

    while let Some(job) = job_rx.recv().await {
        let started = Instant::now();

        // Drain a previously-timed-out turn's pending result first, bounded so a
        // truly wedged claude can't block the worker forever.
        if owes_result {
            let drained = drain_pending_result(&mut reader, &mut session_id, &session).await;
            owes_result = false;
            if drained.is_err() {
                // Stream is unrecoverable (claude died). Fail this job clearly.
                let _ = job.reply_tx.send(Response::error(
                    "claude stream desynced and could not recover (process died?)",
                ));
                break;
            }
        }

        let outcome = run_one_turn(
            &mut stdin,
            &mut reader,
            &job.req,
            &mut session_id,
            &session,
        )
        .await;
        // A timeout leaves the turn's result pending; remember to drain it next.
        if outcome.timed_out {
            owes_result = true;
        }
        let resp = outcome
            .response
            .unwrap_or_else(|e| Response::error(format!("turn failed: {e:#}")));
        let _ = job.reply_tx.send(Response {
            turn_ms: started.elapsed().as_millis() as u64,
            ..resp
        });
    }
}

/// The result of attempting one turn. `timed_out` tells the worker the turn's
/// `result` is still pending on the stream and must be drained before the next.
struct TurnOutcome {
    response: Result<Response>,
    timed_out: bool,
}

/// Drive exactly one turn: write the user envelope, then read stdout lines until
/// the `result` event for this turn arrives (or the timeout fires).
async fn run_one_turn(
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    req: &Request,
    session_id: &mut Option<String>,
    session: &str,
) -> TurnOutcome {
    // Build and write the stream-json user envelope (one line).
    let envelope = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": req.text }
    });
    let line = match serde_json::to_string(&envelope) {
        Ok(mut l) => {
            l.push('\n');
            l
        }
        Err(e) => {
            return TurnOutcome {
                response: Err(anyhow!("serializing envelope: {e}")),
                timed_out: false,
            }
        }
    };
    if let Err(e) = stdin.write_all(line.as_bytes()).await {
        return TurnOutcome {
            response: Err(anyhow!("writing to claude stdin: {e}")),
            timed_out: false,
        };
    }
    if let Err(e) = stdin.flush().await {
        return TurnOutcome {
            response: Err(anyhow!("flushing claude stdin: {e}")),
            timed_out: false,
        };
    }

    // Read events until the `result` sentinel, bounded by the request timeout.
    let deadline = Duration::from_millis(req.timeout_ms.max(1));
    let read_fut = read_until_result(reader, session_id, session);
    match timeout(deadline, read_fut).await {
        Ok(Ok(resp)) => TurnOutcome {
            response: Ok(resp),
            timed_out: false,
        },
        Ok(Err(e)) => TurnOutcome {
            response: Ok(Response::error(format!("read error: {e:#}"))),
            timed_out: false,
        },
        Err(_) => TurnOutcome {
            response: Ok(Response::error(format!(
                "timeout after {}ms waiting for claude result",
                req.timeout_ms
            ))),
            timed_out: true,
        },
    }
}

/// Drain a previously-timed-out turn's still-pending output up to and including
/// its `result` event. Bounded by a generous cap so a wedged claude can't hang
/// the worker indefinitely. Returns Err only if the stream is dead.
async fn drain_pending_result(
    reader: &mut BufReader<ChildStdout>,
    session_id: &mut Option<String>,
    session: &str,
) -> Result<()> {
    // Generous bound: a turn that timed out at, say, 50ms might still legitimately
    // run for a while. Cap the drain so we never block forever on a dead stream.
    let cap = Duration::from_secs(120);
    match timeout(cap, read_until_result(reader, session_id, session)).await {
        Ok(Ok(_)) => Ok(()),       // consumed the stale result; stream realigned
        Ok(Err(e)) => Err(e),      // stdout closed -> claude died
        Err(_) => Err(anyhow!("drain exceeded {cap:?}; claude appears wedged")),
    }
}

/// Read stdout lines, parsing each as a stream-json event, until the turn's
/// `result` event. Updates the cached session id from `init`/`result` events.
async fn read_until_result(
    reader: &mut BufReader<ChildStdout>,
    session_id: &mut Option<String>,
    session: &str,
) -> Result<Response> {
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader
            .read_line(&mut buf)
            .await
            .context("reading claude stdout")?;
        if n == 0 {
            return Err(anyhow!("claude stdout closed mid-turn (process died?)"));
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let ev: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            // Non-JSON noise on stdout is unexpected but non-fatal; skip it.
            Err(_) => continue,
        };
        let ty = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Learn/refresh the session id as soon as it appears, and persist it so
        // `status` and `--resume` recovery can see it.
        if let Some(sid) = ev.get("session_id").and_then(|v| v.as_str()) {
            if session_id.as_deref() != Some(sid) {
                *session_id = Some(sid.to_string());
                persist_session_id(session, sid).await;
            }
        }

        if ty == "result" {
            let is_error = ev
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let text = ev
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let sid = session_id.clone().unwrap_or_default();
            if is_error {
                let subtype = ev
                    .get("subtype")
                    .and_then(|v| v.as_str())
                    .unwrap_or("error");
                return Ok(Response {
                    ok: false,
                    text,
                    session_id: sid,
                    turn_ms: 0,
                    error: Some(format!("claude result error: {subtype}")),
                });
            }
            return Ok(Response {
                ok: true,
                text,
                session_id: sid,
                turn_ms: 0,
                error: None,
            });
        }
        // Other event types (system/init, assistant, rate_limit_event, ...) are
        // informational for this thin pipe; we wait for `result`.
    }
}

/// Handle one client connection: read a single request line, hand it to the
/// worker, write back the single response line.
async fn handle_conn(stream: UnixStream, job_tx: mpsc::Sender<Job>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(()); // client hung up with no request
    }

    let resp = match serde_json::from_str::<Request>(line.trim()) {
        Ok(req) => {
            let (reply_tx, reply_rx) = oneshot::channel();
            if job_tx.send(Job { req, reply_tx }).await.is_err() {
                Response::error("daemon worker is gone")
            } else {
                reply_rx
                    .await
                    .unwrap_or_else(|_| Response::error("worker dropped reply"))
            }
        }
        Err(e) => Response::error(format!("bad request json: {e}")),
    };

    let mut out = serde_json::to_string(&resp)?;
    out.push('\n');
    write_half.write_all(out.as_bytes()).await?;
    write_half.flush().await?;
    Ok(())
}

/// Persist just the session id into the state file (merging over existing).
async fn persist_session_id(session: &str, sid: &str) {
    let path = state_path(session);
    let mut state: State = match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(State {
            pid: std::process::id(),
            session_id: None,
            model: None,
        }),
        Err(_) => State {
            pid: std::process::id(),
            session_id: None,
            model: None,
        },
    };
    state.session_id = Some(sid.to_string());
    let _ = write_state(session, &state).await;
}

/// Atomically write the session state file.
async fn write_state(session: &str, state: &State) -> Result<()> {
    let path = state_path(session);
    let tmp = with_suffix(&path, ".tmp");
    let json = serde_json::to_vec_pretty(state)?;
    tokio::fs::write(&tmp, &json).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

fn with_suffix(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// Background-spawn a fresh `claude-pipe up` (without --detach) and wait for the
/// socket to come up. Used by `up --detach`.
async fn spawn_detached(
    session: &str,
    model: &Option<String>,
    system: &Option<String>,
    resume: &Option<String>,
    full: bool,
) -> Result<()> {
    ensure_runtime_dir().await?;
    let exe = std::env::current_exe().context("locating own exe")?;
    let mut cmd = Command::new(exe);
    cmd.arg("up").arg("--session").arg(session);
    if let Some(m) = model {
        cmd.arg("--model").arg(m);
    }
    if let Some(s) = system {
        cmd.arg("--system").arg(s);
    }
    if let Some(r) = resume {
        cmd.arg("--resume").arg(r);
    }
    if full {
        cmd.arg("--full");
    }
    // Detach from this process group / shell.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            // New session so it survives the parent shell exiting.
            if libc_setsid() == -1 {
                // Non-fatal: still usable if already a session leader.
            }
            Ok(())
        });
    }
    let _child = cmd.spawn().context("spawning detached daemon")?;

    // Wait (briefly) for the socket to appear and accept connections.
    let sock = socket_path(session);
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if UnixStream::connect(&sock).await.is_ok() {
            println!("claude-pipe: session '{session}' up (detached)");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow!(
        "detached daemon did not become ready within 15s (socket {})",
        sock.display()
    ))
}

extern "C" {
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
}
