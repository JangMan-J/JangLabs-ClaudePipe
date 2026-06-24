//! The stateless control CLI client (spec §6.2) — the orchestrator-facing,
//! print-and-exit, scriptable surface. Every verb opens the control socket,
//! sends one [`ControlRequest`] line, prints the reply, and exits. `events`
//! streams until interrupted.
//!
//! This is rigorously **out-of-band**: it speaks the control protocol on
//! `control.sock`, never the data socket. `attach` *prints the data-socket path*
//! to stdout — that path is what the orchestrator points its stock ACP client at
//! (the data path stays pure ACP, Invariant 7).

use crate::protocol::{
    control_socket_path, ControlRequest, ControlResponse,
};
use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Connect to the supervisor's control socket.
async fn connect() -> Result<UnixStream> {
    let path = control_socket_path();
    UnixStream::connect(&path).await.with_context(|| {
        format!(
            "connecting to supervisor at {} (is it up? `claude-pipe serve`)",
            path.display()
        )
    })
}

/// Send one request, read exactly one response line.
async fn round_trip(req: &ControlRequest) -> Result<ControlResponse> {
    let stream = connect().await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).await?;
    let resp: ControlResponse = serde_json::from_str(resp_line.trim())
        .with_context(|| format!("parsing control response: {resp_line:?}"))?;
    Ok(resp)
}

/// A stable holder tag for this CLI invocation: the orchestrator process can pass
/// its own via `--holder`; otherwise we derive one from the parent pid so a shell
/// session is a consistent holder across calls.
fn default_holder() -> String {
    let ppid = unsafe { libc_getppid() };
    format!("cli-{ppid}")
}

/// `claude-pipe list` — enumerate agents.
pub async fn run_list(json: bool) -> Result<()> {
    match round_trip(&ControlRequest::List).await? {
        ControlResponse::Agents { agents } => {
            if json {
                println!("{}", serde_json::to_string(&agents)?);
            } else if agents.is_empty() {
                println!("(no agents)");
            } else {
                println!("{:<16} {:<16} {:<8} {:<10} {:<14} SESSIONS", "ID", "RECIPE", "PID", "LIVENESS", "LEASE");
                for a in agents {
                    println!(
                        "{:<16} {:<16} {:<8} {:<10?} {:<14} {}",
                        a.id,
                        a.recipe,
                        a.pid,
                        a.liveness,
                        a.lease_holder.as_deref().unwrap_or("-"),
                        a.session_count
                    );
                }
            }
            Ok(())
        }
        ControlResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected reply: {other:?}")),
    }
}

/// `claude-pipe spawn <recipe>` — start a new agent.
pub async fn run_spawn(recipe: String, json: bool) -> Result<()> {
    match round_trip(&ControlRequest::Spawn { recipe }).await? {
        ControlResponse::Spawned { agent } => {
            if json {
                println!("{}", serde_json::to_string(&agent)?);
            } else {
                println!("spawned {} (recipe {}, pid {})", agent.id, agent.recipe, agent.pid);
            }
            Ok(())
        }
        ControlResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected reply: {other:?}")),
    }
}

/// `claude-pipe attach <name|id>` — grant the lease and PRINT the data-socket
/// path (spec §6.2). The orchestrator connects its stock ACP client to that path.
pub async fn run_attach(target: String, holder: Option<String>, json: bool) -> Result<()> {
    let holder = holder.unwrap_or_else(default_holder);
    match round_trip(&ControlRequest::Attach { target, holder }).await? {
        ControlResponse::Attached { socket, agent } => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "socket": socket, "agent": agent
                    }))?
                );
            } else {
                // The path on stdout is the whole point — scriptable: a caller does
                // `sock=$(claude-pipe attach gemini)` and connects there.
                println!("{}", socket.display());
            }
            Ok(())
        }
        ControlResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected reply: {other:?}")),
    }
}

/// `claude-pipe detach [<id>]` — release the caller's lease.
pub async fn run_detach(id: Option<String>, holder: Option<String>) -> Result<()> {
    let holder = holder.unwrap_or_else(default_holder);
    match round_trip(&ControlRequest::Detach { id, holder }).await? {
        ControlResponse::Ok { message } => {
            println!("{message}");
            Ok(())
        }
        ControlResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected reply: {other:?}")),
    }
}

/// `claude-pipe kill <id>` — terminate a pooled agent.
pub async fn run_kill(id: String) -> Result<()> {
    match round_trip(&ControlRequest::Kill { id }).await? {
        ControlResponse::Ok { message } => {
            println!("{message}");
            Ok(())
        }
        ControlResponse::Error { message } => Err(anyhow!(message)),
        other => Err(anyhow!("unexpected reply: {other:?}")),
    }
}

/// `claude-pipe events --agent X` — stream read-only telemetry lines until
/// interrupted (Ctrl-C / EOF). Each line is a JSON `Telemetry` sample.
pub async fn run_events(agent: String) -> Result<()> {
    let stream = connect().await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut line = serde_json::to_string(&ControlRequest::Events { agent })?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            return Ok(()); // supervisor closed the stream (agent gone)
        }
        match serde_json::from_str::<ControlResponse>(buf.trim()) {
            Ok(ControlResponse::Telemetry(t)) => {
                println!("{}", serde_json::to_string(&t)?);
            }
            Ok(ControlResponse::Error { message }) => {
                return Err(anyhow!(message));
            }
            Ok(other) => eprintln!("claude-pipe: unexpected events frame: {other:?}"),
            Err(e) => eprintln!("claude-pipe: bad events line {buf:?}: {e}"),
        }
    }
}

extern "C" {
    #[link_name = "getppid"]
    fn libc_getppid() -> i32;
}
