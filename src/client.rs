//! The client side: connect to a session's socket and run a single op.
//!
//! `send` is the one consumers care about — it blocks until the turn completes
//! and prints the reply text (or the full JSON envelope with `--json`).

use crate::protocol::{socket_path, state_path, Request, Response, State};
use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// `claude-pipe send` — one request/reply round-trip.
pub async fn run_send(
    session: String,
    text: Option<String>,
    timeout_ms: u64,
    json: bool,
) -> Result<()> {
    // Text from the argument, else from stdin (so hooks can pipe transcripts).
    let text = match text {
        Some(t) => t,
        None => {
            let mut s = String::new();
            tokio::io::stdin()
                .read_to_string(&mut s)
                .await
                .context("reading message from stdin")?;
            s.trim_end_matches('\n').to_string()
        }
    };

    let resp = request(&session, Request { text, timeout_ms }).await?;

    if json {
        println!("{}", serde_json::to_string(&resp)?);
    } else {
        // Reply text on stdout; a non-ok turn is surfaced on stderr + exit code
        // so a caller can distinguish "Claude said nothing" from "it failed".
        if resp.ok {
            print!("{}", resp.text);
            if !resp.text.ends_with('\n') {
                println!();
            }
        } else {
            eprintln!(
                "claude-pipe: {}",
                resp.error.as_deref().unwrap_or("unknown error")
            );
            std::process::exit(1);
        }
    }
    Ok(())
}

/// `claude-pipe down` — ask the daemon to stop by killing its pid.
pub async fn run_down(session: String) -> Result<()> {
    let state = read_state(&session).await;
    match state {
        Some(s) => {
            // SIGTERM the daemon; its signal handler tears down cleanly.
            let killed = unsafe { libc_kill(s.pid as i32, 15) } == 0;
            if killed {
                println!("claude-pipe: sent SIGTERM to session '{session}' (pid {})", s.pid);
                Ok(())
            } else {
                // Process already gone; clean up stale files.
                let _ = tokio::fs::remove_file(socket_path(&session)).await;
                let _ = tokio::fs::remove_file(state_path(&session)).await;
                println!("claude-pipe: session '{session}' was not running (cleaned up)");
                Ok(())
            }
        }
        None => Err(anyhow!("no such session '{session}' (no state file)")),
    }
}

/// `claude-pipe status` — report liveness + session id.
pub async fn run_status(session: String) -> Result<()> {
    let sock = socket_path(&session);
    let live = UnixStream::connect(&sock).await.is_ok();
    let state = read_state(&session).await;
    match state {
        Some(s) => {
            println!("session:    {session}");
            println!("live:       {live}");
            println!("pid:        {}", s.pid);
            println!("session_id: {}", s.session_id.as_deref().unwrap_or("(pending)"));
            println!("model:      {}", s.model.as_deref().unwrap_or("(default)"));
            println!("socket:     {}", sock.display());
        }
        None => {
            println!("session:    {session}");
            println!("live:       {live}");
            println!("(no state file)");
        }
    }
    Ok(())
}

/// Open the socket, send one Request line, read one Response line.
async fn request(session: &str, req: Request) -> Result<Response> {
    let sock = socket_path(session);
    let stream = UnixStream::connect(&sock).await.with_context(|| {
        format!(
            "connecting to session '{session}' at {} (is the daemon up? `claude-pipe up`)",
            sock.display()
        )
    })?;
    let (read_half, mut write_half) = stream.into_split();

    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;
    // Signal EOF on the write side so the daemon's read_line returns promptly.
    write_half.shutdown().await.ok();

    let mut reader = BufReader::new(read_half);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).await?;
    let resp: Response = serde_json::from_str(resp_line.trim())
        .with_context(|| format!("parsing daemon response: {resp_line:?}"))?;
    Ok(resp)
}

async fn read_state(session: &str) -> Option<State> {
    let bytes = tokio::fs::read(state_path(session)).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
