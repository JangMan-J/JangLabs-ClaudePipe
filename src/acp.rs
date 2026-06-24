//! Semantics-blind ACP frame inspection — claude-pipe's **entire** knowledge of
//! the protocol it transports.
//!
//! Spec Invariant 2 (semantics-blindness): claude-pipe MUST NOT parse, interpret,
//! or act on ACP method semantics. It MAY read **only** `sessionId` (routing /
//! fairness) and track **turn-open/closed** per session (steal safety). It MUST
//! NOT distinguish `session/prompt` from `fs/read_text_file` *except* insofar as
//! detecting that one specific method opens a turn.
//!
//! Everything here operates on a single newline-delimited JSON-RPC frame and
//! extracts at most those two facts. **The relay never forwards the parsed form**
//! — it forwards the original bytes verbatim (Invariant 1). This module only
//! *looks*; it never *rewrites*.
//!
//! ACP v1 wire facts this relies on (verified, Context7 `agent-client-protocol`):
//!   - newline-delimited JSON-RPC 2.0, one message per line, no embedded newlines.
//!   - `sessionId` is at `params.sessionId` for client→agent methods and
//!     agent→client `session/update` notifications; at `result.sessionId` for the
//!     `session/new` response (where a new id is minted).
//!   - a turn opens on client→agent `{"method":"session/prompt","id":N,
//!     "params":{"sessionId":S}}` and closes on the agent's response to that same
//!     `id` — `{"id":N,"result":{"stopReason":…}}` (the close carries only `id`).

use serde_json::Value;

/// What a single inspected frame tells the relay. All fields are best-effort:
/// a frame we cannot parse (or that lacks these fields) yields an all-`None`
/// inspection and is still relayed byte-faithfully — we simply learn nothing
/// from it (which is correct: an unparseable line is the agent's business).
#[derive(Debug, Default, Clone)]
pub struct FrameInfo {
    /// The `sessionId` this frame is routed by, if present (`params.sessionId`
    /// or `result.sessionId`). Used for per-session demux + fairness.
    pub session_id: Option<String>,
    /// The JSON-RPC `id`, if this frame carries one (request or response).
    pub id: Option<RpcId>,
    /// `Some(true)` iff this is a `session/prompt` *request* (opens a turn).
    pub is_prompt_request: bool,
    /// `Some(stopReason)` iff this is a *response* carrying `result.stopReason`
    /// (closes a turn). The string is the stopReason value, relayed-as-is; we
    /// only need its presence, but we surface it for telemetry/logging.
    pub stop_reason: Option<String>,
}

/// A JSON-RPC id is either a number or a string per the spec. We keep it as an
/// owned, hashable key so we can map a `session/prompt` request id to its
/// session and recognize the matching response.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RpcId {
    Num(i64),
    Str(String),
}

impl RpcId {
    fn from_value(v: &Value) -> Option<RpcId> {
        match v {
            Value::Number(n) => n.as_i64().map(RpcId::Num),
            Value::String(s) => Some(RpcId::Str(s.clone())),
            _ => None,
        }
    }
}

/// Inspect one frame (the raw bytes of a single newline-delimited JSON-RPC
/// message, with or without the trailing newline). Returns what little the relay
/// is permitted to know. Never mutates and never fails loudly — an unparseable
/// frame yields a default [`FrameInfo`].
pub fn inspect(frame: &[u8]) -> FrameInfo {
    let v: Value = match serde_json::from_slice(frame) {
        Ok(v) => v,
        Err(_) => return FrameInfo::default(),
    };
    let obj = match v.as_object() {
        Some(o) => o,
        None => return FrameInfo::default(),
    };

    let mut info = FrameInfo::default();

    // --- id (request or response) ---------------------------------------
    if let Some(id) = obj.get("id") {
        info.id = RpcId::from_value(id);
    }

    // --- sessionId: params.sessionId OR result.sessionId ----------------
    // (params for requests/notifications; result for the session/new response,
    // where the id is minted.)
    let sid = obj
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .or_else(|| obj.get("result").and_then(|r| r.get("sessionId")))
        .and_then(|s| s.as_str());
    if let Some(sid) = sid {
        info.session_id = Some(sid.to_string());
    }

    // --- turn-open: a session/prompt REQUEST ----------------------------
    // The one and only method name we recognize. This is the minimum peek the
    // spec permits (§9) — purely to gate steal safety, never to act on payload.
    if obj.get("method").and_then(|m| m.as_str()) == Some("session/prompt") {
        info.is_prompt_request = true;
    }

    // --- turn-close: a RESPONSE carrying result.stopReason --------------
    if let Some(sr) = obj
        .get("result")
        .and_then(|r| r.get("stopReason"))
        .and_then(|s| s.as_str())
    {
        info.stop_reason = Some(sr.to_string());
    }

    info
}

/// Split a buffer into complete newline-terminated frames, returning the frames
/// (each *including* its trailing `\n`) and the number of bytes consumed. A
/// trailing partial line (no newline yet) is left unconsumed for the next read.
///
/// This is the framing the relay uses to know where one JSON-RPC message ends —
/// the ACP guarantee that messages are newline-delimited and contain no embedded
/// newlines is what makes this correct (and what makes a frame impossible to
/// deliver *partially*, satisfying Invariant 1 by construction).
pub fn split_frames(buf: &[u8]) -> (Vec<&[u8]>, usize) {
    let mut frames = Vec::new();
    let mut start = 0;
    let mut consumed = 0;
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' {
            frames.push(&buf[start..=i]);
            start = i + 1;
            consumed = start;
        }
    }
    (frames, consumed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_session_id_from_params() {
        let f = br#"{"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":"sess_abc","prompt":[]}}"#;
        let info = inspect(f);
        assert_eq!(info.session_id.as_deref(), Some("sess_abc"));
        assert!(info.is_prompt_request);
        assert_eq!(info.id, Some(RpcId::Num(2)));
        assert!(info.stop_reason.is_none());
    }

    #[test]
    fn extracts_session_id_from_result() {
        // session/new response mints the id in `result`.
        let f = br#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"sess_new"}}"#;
        let info = inspect(f);
        assert_eq!(info.session_id.as_deref(), Some("sess_new"));
        assert_eq!(info.id, Some(RpcId::Num(1)));
        assert!(!info.is_prompt_request);
    }

    #[test]
    fn detects_stop_reason_close() {
        let f = br#"{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}"#;
        let info = inspect(f);
        assert_eq!(info.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(info.id, Some(RpcId::Num(2)));
        // No sessionId on the close frame — that's why we need the id→session map.
        assert!(info.session_id.is_none());
    }

    #[test]
    fn session_update_notification_has_session_no_id() {
        let f = br#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess_x","update":{"sessionUpdate":"agent_message_chunk"}}}"#;
        let info = inspect(f);
        assert_eq!(info.session_id.as_deref(), Some("sess_x"));
        assert!(info.id.is_none());
        assert!(!info.is_prompt_request);
    }

    #[test]
    fn string_ids_supported() {
        let f = br#"{"jsonrpc":"2.0","id":"req-7","method":"session/prompt","params":{"sessionId":"s"}}"#;
        let info = inspect(f);
        assert_eq!(info.id, Some(RpcId::Str("req-7".into())));
    }

    #[test]
    fn fs_request_is_not_a_prompt() {
        // A server-initiated fs/read_text_file must NOT be mistaken for a turn open.
        let f = br#"{"jsonrpc":"2.0","id":9,"method":"fs/read_text_file","params":{"sessionId":"s","path":"/x"}}"#;
        let info = inspect(f);
        assert!(!info.is_prompt_request);
        assert_eq!(info.session_id.as_deref(), Some("s"));
    }

    #[test]
    fn unparseable_frame_yields_default() {
        let info = inspect(b"this is not json\n");
        assert!(info.session_id.is_none());
        assert!(info.id.is_none());
        assert!(!info.is_prompt_request);
    }

    #[test]
    fn split_frames_handles_partial_trailing() {
        let buf = b"{\"a\":1}\n{\"b\":2}\n{\"partial\":";
        let (frames, consumed) = split_frames(buf);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], b"{\"a\":1}\n");
        assert_eq!(frames[1], b"{\"b\":2}\n");
        assert_eq!(consumed, 16); // both complete lines; partial left for next read
    }

    #[test]
    fn split_frames_empty_on_no_newline() {
        let (frames, consumed) = split_frames(b"{\"no\":\"newline yet\"}");
        assert!(frames.is_empty());
        assert_eq!(consumed, 0);
    }
}
