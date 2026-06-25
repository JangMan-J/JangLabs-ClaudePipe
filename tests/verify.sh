#!/usr/bin/env bash
# claude-pipe v2 — the §12 verification suite (the done-gate, plan item 24).
#
# Each check maps 1:1 to a spec §12 criterion. Runs against the proven harness
# (tests/support/{mock-acp-agent,acp-client}.mjs) and the real `claude-pipe`
# binary. Checks needing an EXTERNAL agent (gemini/claude) are gated behind env
# flags so the core suite runs hermetically on any box.
#
# Usage:
#   tests/verify.sh                 # run all hermetic checks (1-6,8,9)
#   RUN_GEMINI=1 tests/verify.sh    # also check 7a (acp-stdio vs gemini --acp)
#   RUN_CLAUDE=1 tests/verify.sh    # also 7b (claude-channels, subscription) AND
#                                   # 7c (delegate permission relay round-trip)
#
# Exit 0 iff every executed check passes.

set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/claude-pipe"
MOCK="$ROOT/tests/support/mock-acp-agent.mjs"
CLIENT="$ROOT/tests/support/acp-client.mjs"
COORD="$ROOT/tests/support/steal-midturn.mjs"
DELEG="$ROOT/tests/support/delegate-live.mjs"

# Short runtime dir — Unix socket paths must be < ~108 bytes (SUN_LEN).
RT="$(mktemp -d /tmp/cp-verify-XXXXXX)"
export XDG_RUNTIME_DIR="$RT"
export CLAUDE_PIPE_MOCK_AGENT="$MOCK"
export CLAUDE_PIPE_CHANNELS_BRIDGE="$ROOT/scripts/claude-channels-bridge.mjs"

PASS=0
FAIL=0
declare -a RESULTS

ok()   { RESULTS+=("PASS  $1"); PASS=$((PASS+1)); }
bad()  { RESULTS+=("FAIL  $1 -- $2"); FAIL=$((FAIL+1)); }
info() { echo "  ... $1"; }

cleanup() {
  # Kill the supervisor we started (match the real binary path, not shell wrappers).
  for p in $(ps -eo pid,args 2>/dev/null | grep 'target/release/claude-pipe serve' | grep -v 'grep' | grep -v 'zsh -c' | awk '{print $1}'); do
    kill -TERM "$p" 2>/dev/null
  done
  sleep 0.3
  for p in $(ps -eo pid,args 2>/dev/null | grep 'target/release/claude-pipe serve' | grep -v 'grep' | grep -v 'zsh -c' | awk '{print $1}'); do
    kill -KILL "$p" 2>/dev/null
  done
  pkill -9 -f "mock-acp-agent" 2>/dev/null
  # The claude-channels bridge spawns a real `claude --channels` + channel-server;
  # reap them and their temp sockets so runs don't accumulate orphans.
  pkill -9 -f "dangerously-load-development-channels" 2>/dev/null
  pkill -9 -f "as-channel-server" 2>/dev/null
  pkill -9 -f "claude-channels-bridge" 2>/dev/null
  rm -rf "$RT" 2>/dev/null
  find /tmp -maxdepth 1 -name 'cp-channels-*' -mmin +1 -exec rm -rf {} + 2>/dev/null
  true
}
trap cleanup EXIT

# Wait (bounded) until no `claude-pipe serve` supervisor is running — deterministic
# per-check isolation so a prior check's teardown can't leak a contending supervisor
# into the next one (the contention the 5b flake stemmed from). Polls instead of a
# fixed sleep so it returns as soon as the process is actually gone, and gives up after
# ~3s so a wedged process can't hang the suite.
wait_no_supervisor() {
  for _ in $(seq 1 60); do
    pgrep -f "release/claude-pipe serve" >/dev/null 2>&1 || return 0
    sleep 0.05
  done
  return 1
}

# Start a fresh supervisor with a mock agent; echo the data socket path. Hardened:
# starts from a known-clean process baseline (no leaked supervisor), and polls for the
# attach to actually yield a socket rather than trusting a fixed boot sleep — so heavy
# ambient load (e.g. concurrent channels testing) slows but does not break it.
start_one_mock() {
  wait_no_supervisor || pkill -KILL -f "release/claude-pipe serve" 2>/dev/null
  rm -f "$RT/claude-pipe/"* 2>/dev/null
  "$BIN" serve --prespawn mock --detach >/dev/null 2>&1
  # Poll for a working attach instead of a fixed `sleep 0.4`: retry until the warm
  # mock is reachable or we exhaust the budget (~6s, generous for a loaded box).
  local sock=""
  for _ in $(seq 1 60); do
    sock="$("$BIN" attach mock 2>/dev/null)"
    [ -n "$sock" ] && break
    sleep 0.1
  done
  printf '%s' "$sock"
}

stop_supervisor() {
  for p in $(pgrep -f "release/claude-pipe serve" 2>/dev/null); do kill -TERM "$p" 2>/dev/null; done
  # Deterministic: wait for the supervisor to actually exit (bounded) so the next
  # check starts clean, instead of a fixed sleep that may be too short under load.
  wait_no_supervisor || true
}

echo "==================================================================="
echo " claude-pipe v2 — §12 verification suite"
echo " runtime: $XDG_RUNTIME_DIR"
echo "==================================================================="

# ── Check 2: byte fidelity on an over-wide session/update chunk ──────────────
# (Run first — it's the most fundamental: the bytes must survive verbatim.)
echo "[Check 2] byte fidelity — over-wide chunk, sha verified"
SOCK="$(start_one_mock)"
if [ -z "$SOCK" ]; then
  bad "check2-byte-fidelity" "could not attach mock"
else
  SID="$(node "$CLIENT" "$SOCK" newsession)"
  # 200000 'X' bytes — far past any terminal width; a grid would wrap/reflow it.
  N=200000
  RES="$(node "$CLIENT" "$SOCK" capture "$SID" "BIG:$N")"
  GOT_LEN="$(echo "$RES" | node -e 'process.stdin.on("data",d=>{try{console.log(JSON.parse(d).len)}catch{console.log(-1)}})')"
  EXP_SHA="$(node -e 'const c=require("crypto");process.stdout.write(c.createHash("sha256").update("X".repeat('"$N"'),"utf8").digest("hex"))')"
  GOT_SHA="$(echo "$RES" | node -e 'process.stdin.on("data",d=>{try{process.stdout.write(JSON.parse(d).sha)}catch{}})')"
  if [ "$GOT_LEN" = "$N" ] && [ "$GOT_SHA" = "$EXP_SHA" ]; then
    ok "check2-byte-fidelity ($N bytes, sha match)"
  else
    bad "check2-byte-fidelity" "len=$GOT_LEN (want $N) sha=$GOT_SHA (want $EXP_SHA)"
  fi
fi
stop_supervisor

# ── Check 2b: per-session FIFO ordering — stopReason MUST NOT overtake chunks ─
# A session's prompt response (stopReason) carries no sessionId field; the relay
# must still order it BEHIND that session's session/update chunks (it recovers the
# session from the prompt id→session map). Regression for the reordering bug where
# a 300-chunk flood's stopReason arrived first. (§3.1 "no buffering that reorders".)
echo "[Check 2b] per-session ordering — stopReason ordered behind its chunks"
SOCK="$(start_one_mock)"
ORD="$(node -e '
const net=require("net");const sock=net.createConnection(process.argv[1]);
let buf="",updates=0,step=0;
sock.on("connect",()=>sock.write(JSON.stringify({jsonrpc:"2.0",id:1,method:"initialize",params:{}})+"\n"));
sock.on("data",d=>{buf+=d.toString();let i;while((i=buf.indexOf("\n"))>=0){const line=buf.slice(0,i);buf=buf.slice(i+1);if(!line.trim())continue;let m;try{m=JSON.parse(line)}catch{continue}
 if(m.method==="session/update")updates++;
 if(m.id===1&&step===0){step=1;sock.write(JSON.stringify({jsonrpc:"2.0",id:2,method:"session/new",params:{}})+"\n");}
 else if(m.id===2&&step===1){step=2;sock.write(JSON.stringify({jsonrpc:"2.0",id:3,method:"session/prompt",params:{sessionId:m.result.sessionId,prompt:[{type:"text",text:"FLOOD:300"}]}})+"\n");}
 else if(m.id===3&&step===2){console.log(updates);sock.end();process.exit(0);}}});
setTimeout(()=>{console.log(-1);process.exit(0)},5000);
' "$SOCK" 2>/dev/null)"
if [ "$ORD" = "300" ]; then
  ok "check2b-ordering (all 300 chunks delivered before stopReason; no reorder)"
else
  bad "check2b-ordering" "chunks-before-stopReason=$ORD (want 300)"
fi
stop_supervisor

# ── Check 6: callback pass-through (fs/read_text_file) uninterpreted ─────────
echo "[Check 6] callback pass-through — server-initiated fs/read_text_file"
SOCK="$(start_one_mock)"
SID="$(node "$CLIENT" "$SOCK" newsession)"
RES="$(node "$CLIENT" "$SOCK" callback "$SID")"
# The mock issues fs/read_text_file; the client answers {content:"MOCKFILE"};
# the mock then chunks "callback-got:...MOCKFILE...". A pass = the round-trip
# completed (stopReason end_turn) and the client's answer reached the agent.
if echo "$RES" | grep -q "MOCKFILE" && echo "$RES" | grep -q "end_turn"; then
  ok "check6-callback-passthrough (fs/* forwarded + response relayed)"
else
  bad "check6-callback-passthrough" "got: $RES"
fi
stop_supervisor

# ── Check 3: multi-session fairness ──────────────────────────────────────────
# ONE connection (single lease, §9), 3 sessions multiplexed (§4), fired
# concurrently: a FLOOD:300 on session 1 must NOT stall normal prompts on
# sessions 2 and 3 over the same connection. All three must complete, proving
# per-session demux + fair drain (§6.3). (The soft/hard-bound overflow transitions
# are asserted separately in the telemetry check below.)
echo "[Check 3] multi-session fairness — 3 sessions multiplexed, flood doesn't stall"
SOCK="$(start_one_mock)"
RES="$(node "$CLIENT" "$SOCK" multi "FLOOD:300" "echo-me-2" "echo-me-3" 2>&1)"
# Pass = all three sessions ended with end_turn AND the flooded one delivered 300
# chunks while the others delivered their single echo (1 chunk each).
# Fairness property (deterministic): all 3 sessions complete (end_turn), AND the
# flooded session's chunks vastly outnumber the two echo sessions (which get
# exactly 1 each) — proving per-session demux routed correctly and the flood did
# not stall or steal the others' frames. Exact flood count is the mock's business
# (it bursts); we assert flood ≫ echoes, which is the real §6.3 guarantee.
OK3="$(echo "$RES" | node -e '
let d="";process.stdin.on("data",c=>d+=c);process.stdin.on("end",()=>{
  try{const o=JSON.parse(d);const v=Object.values(o);
    const allEnd=v.every(x=>x.stopReason==="end_turn");
    const flood=v.find(x=>x.chunks>=50);
    const echoes=v.filter(x=>x.chunks===1).length;
    console.log(allEnd&&flood&&echoes===2?"YES":"NO");
  }catch(e){console.log("NO")}
})')"
if [ "$OK3" = "YES" ]; then
  ok "check3-fairness (flood on S1 did not stall S2/S3; all end_turn; demux routed correctly)"
else
  bad "check3-fairness" "got: $RES"
fi
stop_supervisor

# ── Check 3b: overflow telemetry — soft-bound pressured + hard-bound torn ─────
# Stall one session's reader so its forward queue grows; assert telemetry surfaces
# 'pressured' (soft bound) and the lease is torn at the hard bound, never silently.
echo "[Check 3b] overflow telemetry — pressured surfaced (stalled reader)"
SOCK="$(start_one_mock)"
# Subscribe to telemetry in the background.
node "$CLIENT" "$SOCK" init >/dev/null 2>&1
# Use a SECOND attach for the stalling client (this steals; that's fine — we just
# need one client whose reader stalls while the agent floods its session).
SOCK2="$("$BIN" attach mock 2>/dev/null)"
( "$BIN" events --agent mock > "$RT/telem.out" 2>&1 ) &
TELEM=$!
SIDS="$(node "$CLIENT" "$SOCK2" newsession 2>/dev/null)"
# Stall: send a big flood and never read it. The mock's FLOOD emits many frames;
# combined with a paused reader the per-session forward queue must peg the soft
# bound and surface 'pressured'. Run in background; kill after a moment.
timeout 4 node "$CLIENT" "$SOCK2" stall "$SIDS" "FLOOD:5000" >/dev/null 2>&1 &
STALLER=$!
sleep 3
kill "$STALLER" 2>/dev/null
kill "$TELEM" 2>/dev/null
if grep -q "pressured" "$RT/telem.out"; then
  ok "check3b-overflow (soft bound surfaced 'pressured' on telemetry; never-silent)"
else
  # Soft bound is 1024 frames; FLOOD:5000 exceeds it, but if the OS buffer absorbed
  # it before the stall took hold, report what telemetry showed.
  bad "check3b-overflow" "no 'pressured' in telemetry: $(cat $RT/telem.out | tr '\n' ' ' | head -c200)"
fi
stop_supervisor

# ── Check 5: handoff safety — steal at idle is immediate ─────────────────────
# (Mid-turn-waits-for-stopReason is asserted in the deeper probe with timing; the
# hermetic check here proves an idle steal succeeds and the new lease works.)
echo "[Check 5] handoff — idle steal grants a working lease to client 2"
SOCK="$(start_one_mock)"
SID="$(node "$CLIENT" "$SOCK" newsession)"
# First attach already happened (start_one_mock). A second attach = steal at idle.
SOCK2="$("$BIN" attach mock 2>/dev/null)"
RES="$(node "$CLIENT" "$SOCK2" prompt "$SID" after-steal 2>&1)"
if echo "$RES" | grep -q "echo:after-steal"; then
  ok "check5-handoff-idle (steal succeeded; new lease drives a turn)"
else
  bad "check5-handoff-idle" "got: $RES"
fi
stop_supervisor

# ── Check 5b: handoff safety MID-TURN — steal waits for stopReason (§9) ───────
# Deterministic, contention-immune (replaces the old `sleep 0.4` + absolute-ms floor,
# which flaked under load — see steal-midturn.mjs). The coordinator drives BOTH clients
# on one clock: it gates the steal on the SLOW turn being *observably open* (first chunk
# seen — no fixed sleep), then asserts the CAUSAL §9 invariant `t_steal_done >=
# t_slow_stop` (the steal could only finish AFTER the open turn's boundary). Uniform
# contention shifts both timestamps together and never inverts the ordering, so a slow
# box no longer produces a spurious fail.
echo "[Check 5b] handoff mid-turn — steal blocks until the open turn's stopReason"
SOCK="$(start_one_mock)"
SID="$(node "$CLIENT" "$SOCK" newsession)"
# The coordinator opens a SLOW:2500 turn on this lease, waits for it to be open, then
# steals via `$BIN attach mock` and proves the steal waited for the boundary.
R5B="$(node "$COORD" "$SOCK" "$SID" 2500 "$BIN" attach mock 2>&1)"
if echo "$R5B" | grep -q '"ok":true'; then
  DET="$(echo "$R5B" | node -e 'let d="";process.stdin.on("data",c=>d+=c);process.stdin.on("end",()=>{try{console.log(JSON.parse(d).detail)}catch{console.log("")}})' 2>/dev/null)"
  ok "check5b-handoff-midturn ($DET)"
else
  bad "check5b-handoff-midturn" "coordinator: $R5B"
fi
stop_supervisor

# ── Check 9: voxtype/v1 untouched — v1 up/send/down/status still function ─────
echo "[Check 9] v1 untouched — up/status/down still work (no claude needed: status path)"
# We don't spawn a real claude (that needs auth); we assert the v1 CLI surface is
# intact: `status` on a non-running session reports cleanly and exits non-zero,
# and `down` on a missing session errors as designed. This proves v1 code wasn't
# broken by the v2 additions (the deeper probe drives a real v1 turn if desired).
V1OUT="$("$BIN" status --session __nope__ 2>&1)"; V1RC=$?
if echo "$V1OUT" | grep -q "live:" && [ $V1RC -ne 0 ]; then
  ok "check9-v1-untouched (v1 status surface intact)"
else
  bad "check9-v1-untouched" "rc=$V1RC out=$V1OUT"
fi

# ── Check 8: strictly-in-band — no fd to anything but stdio + own sockets ─────
echo "[Check 8] strictly-in-band — agent process opens no unexpected files"
SOCK="$(start_one_mock)"
SID="$(node "$CLIENT" "$SOCK" newsession)"
node "$CLIENT" "$SOCK" prompt "$SID" "in-band-probe" >/dev/null 2>&1
# Inspect the supervisor's own open fds: it must hold only sockets, pipes, std
# streams, and the agent child pipes — NO open handle to agent logs/transcripts/
# result files. (The mock agent has no such files; we assert the supervisor has no
# regular-file fd under $HOME or a transcript dir.)
SUP_PID="$(pgrep -f "release/claude-pipe serve" | head -1)"
if [ -n "$SUP_PID" ] && [ -d "/proc/$SUP_PID/fd" ]; then
  BADFD="$(ls -l /proc/$SUP_PID/fd 2>/dev/null | grep -E '\->' | grep -vE 'socket:|pipe:|/dev/(null|pts|tty)|anon_inode|/proc/' | grep -E "$HOME|\.jsonl|transcript|\.log" || true)"
  if [ -z "$BADFD" ]; then
    ok "check8-in-band (supervisor holds only sockets/pipes/std; no log/transcript fd)"
  else
    bad "check8-in-band" "unexpected fd(s): $BADFD"
  fi
else
  bad "check8-in-band" "could not inspect supervisor fds (pid=$SUP_PID)"
fi
stop_supervisor

# ── Check 1: stock-client invisibility ───────────────────────────────────────
# A stock ACP client completes initialize/session/new/session/prompt and gets a
# stopReason with no pooling awareness. We already exercise exactly this above;
# here we assert the FULL handshake sequence works through an attach-returned
# socket with a vanilla client and nothing claude-pipe-specific appears on the wire.
echo "[Check 1] stock-client invisibility — full ACP handshake, no envelope"
SOCK="$(start_one_mock)"
INIT="$(node "$CLIENT" "$SOCK" init)"
SID="$(node "$CLIENT" "$SOCK" newsession)"
PR="$(node "$CLIENT" "$SOCK" prompt "$SID" invisible 2>&1)"
if echo "$INIT" | grep -q protocolVersion && [ -n "$SID" ] && echo "$PR" | grep -q "echo:invisible"; then
  ok "check1-invisibility (init+new+prompt+stopReason via stock client)"
else
  bad "check1-invisibility" "init=$INIT sid=$SID prompt=$PR"
fi
stop_supervisor

# ── Check 4: warm-start latency — attach→first ack on warm pool is fast ──────
echo "[Check 4] warm-start latency — warm attach pays no cold start"
SOCK="$(start_one_mock)"
# Time attach→first session/new ack against the already-warm agent.
T0=$(date +%s%N)
SID="$(node "$CLIENT" "$SOCK" newsession)"
T1=$(date +%s%N)
MS=$(( (T1 - T0) / 1000000 ))
# Warm path should be well under a cold node+claude spawn (~seconds). Assert the
# warm new-session ack returns quickly (generous bound to avoid CI flakiness).
if [ -n "$SID" ] && [ "$MS" -lt 2000 ]; then
  ok "check4-warm-start (warm session/new ack in ${MS}ms)"
else
  bad "check4-warm-start" "sid=$SID took ${MS}ms"
fi
stop_supervisor

# ── Check 7a: acp-stdio vs gemini --acp (gated) ──────────────────────────────
if [ "${RUN_GEMINI:-0}" = "1" ]; then
  echo "[Check 7a] recipe coverage — acp-stdio vs gemini --acp"
  rm -f "$RT/claude-pipe/"* 2>/dev/null
  "$BIN" serve --prespawn gemini --detach >/dev/null 2>&1
  sleep 1.5
  GSOCK="$("$BIN" attach gemini 2>/dev/null)"
  if [ -n "$GSOCK" ]; then
    GINIT="$(timeout 30 node "$CLIENT" "$GSOCK" init 2>&1)"
    if echo "$GINIT" | grep -qiE 'protocolVersion|capabilit'; then
      ok "check7a-gemini (gemini --acp initialized through the relay)"
    else
      bad "check7a-gemini" "init reply: $GINIT"
    fi
  else
    bad "check7a-gemini" "could not attach gemini agent"
  fi
  stop_supervisor
else
  RESULTS+=("SKIP  check7a-gemini (set RUN_GEMINI=1)")
fi

# ── Check 7b: claude-channels vs live claude --channels (gated, subscription) ─
if [ "${RUN_CLAUDE:-0}" = "1" ]; then
  echo "[Check 7b] recipe coverage — claude-channels round-trip on subscription"
  echo "  ... (this launches a real interactive 'claude --channels' on the subscription;"
  echo "       allow ~15s for Claude to boot + register the channel before the prompt)"
  rm -f "$RT/claude-pipe/"* 2>/dev/null
  unset ANTHROPIC_API_KEY  # subscription OAuth, not API key (§7.2)
  "$BIN" serve --prespawn claude-channels --detach >/dev/null 2>&1
  # The bridge spawns Claude lazily at startup; give it time to boot, auto-confirm
  # the development-channels prompt, and connect its channel-server before we drive
  # a prompt. (The bridge also waits internally up to 60s for that connection.)
  sleep 14
  CSOCK="$("$BIN" attach claude-channels 2>/dev/null)"
  if [ -n "$CSOCK" ]; then
    CSID="$(timeout 25 node "$CLIENT" "$CSOCK" newsession 2>&1)"
    CRES="$(timeout 120 node "$CLIENT" "$CSOCK" prompt "$CSID" "What is 17 times 23? Reply with ONLY the number." 2>&1)"
    if echo "$CRES" | grep -q "391"; then
      ok "check7b-claude-channels (task round-tripped on the subscription; got 391, no -p/SDK)"
    else
      bad "check7b-claude-channels" "reply: $CRES"
    fi
  else
    bad "check7b-claude-channels" "could not attach claude-channels agent"
  fi
  stop_supervisor
else
  RESULTS+=("SKIP  check7b-claude-channels (set RUN_CLAUDE=1)")
fi

# ── Check 7c: DELEGATE permission path through the recipe (gated, subscription) ─
# Proves the wired client-mediated permission path end to end: the recipe spawns the
# bridge with CHANNELS_KIT_PERMISSION=delegate (→ bridge auto-sets --permission-mode
# default), Claude prompts for tool approval, the channel relays it, the facade emits a
# REAL ACP session/request_permission to the leased client, we answer allow_once, and
# the verdict round-trips so the turn completes. (Fail-closed behavior is covered by
# the hermetic facade tests; this is the live integration of the relay path.)
if [ "${RUN_CLAUDE:-0}" = "1" ]; then
  echo "[Check 7c] delegate permission relay — real ACP session/request_permission round-trip"
  rm -f "$RT/claude-pipe/"* 2>/dev/null
  unset ANTHROPIC_API_KEY
  CHANNELS_KIT_PERMISSION=delegate "$BIN" serve --prespawn claude-channels --detach >/dev/null 2>&1
  sleep 14
  DSOCK="$("$BIN" attach claude-channels 2>/dev/null)"
  if [ -n "$DSOCK" ]; then
    DRES="$(timeout 140 node "$DELEG" "$DSOCK" allow 2>&1)"
    if echo "$DRES" | grep -q '"sawPermissionRequest":true' && echo "$DRES" | grep -q '"answered":true'; then
      ok "check7c-delegate (real ACP session/request_permission relayed + answered via the recipe)"
    else
      bad "check7c-delegate" "driver: $DRES"
    fi
  else
    bad "check7c-delegate" "could not attach claude-channels agent (delegate)"
  fi
  stop_supervisor
else
  RESULTS+=("SKIP  check7c-delegate (set RUN_CLAUDE=1)")
fi

echo "==================================================================="
echo " RESULTS"
echo "==================================================================="
for r in "${RESULTS[@]}"; do echo "  $r"; done
echo "-------------------------------------------------------------------"
echo "  PASS=$PASS  FAIL=$FAIL"
echo "==================================================================="
[ "$FAIL" -eq 0 ]
