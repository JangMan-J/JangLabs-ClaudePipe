# Reconciliation: Transport-Verb-Tiers Advisory × Measurement-Pass Results

**Inputs:** `~/JangLabs/.handoffs/.claude-pipe/transport-verb-tiers-advisory-handoff-20260701.md`
(strong advisory from the jskills-side Fable session; its own rule: *where it disagrees with
the parity results, the parity results win*) × `docs/lean-parity-measurement-pass-20260701.md`
(the measured ledger). Written 2026-07-02. Advisory references spot-checked on disk
(bsoup seam :273-281, ADR-0003, codex-exec-wrapper, runner-adapters) — all present as cited.

**Bottom line:** the two documents agree on ~90% of the surface, and the agreement is
*independent* (the advisory derives from jskills failure history; the ledger from live
gates). Three genuine conflicts exist; all three trace to ONE stale assumption in the
advisory — that codex's backend is cold `exec`+resume. The pass flipped that (warm
app-server, 2.34×), and each conflict resolves cleanly under the flip. Plus the advisory
contributes two things the ledger lacked: an `alive` verb and a consumer-demand lens.

---

## 1. Convergences (independent arrival — highest-confidence layer)

| Advisory | Ledger/spec | Note |
|---|---|---|
| Tier-1 `spawn(model, system)` — fold system injection into spawn, kill `prime` | skeleton `init(engine, opts)` + c1/c2/c3 as **init-time** params | Exactly how the arms were measured; c3's channels are spawn-scoped on both engines (`--system-prompt` / AGENTS.md-or-`thread/start.baseInstructions`) |
| Tier-1 durable session identity | c4 IN (claude keeps same session_id on resume; codex thread-id) | See §3.4 — warmth adds one unmeasured obligation |
| Tier-1 fused `turn(handle, text, timeout)` | skeleton `send`+`recv` "may be fused"; timeout inside the verb; TURN_TIMEOUT in the §1.1 taxonomy | The v1 `--timeout-ms` shape both docs cite |
| Tier-1/2 `stop`/`alive` as **turn-acceptability**, not process-liveness | §1.1 "ready = a first send would succeed"; reinforced by the pass's finding that claude's `init` event is NOT a readiness signal | Same semantics, two derivations |
| Tier-2 typed failure never content | §1.1 error taxonomy (5 outcomes); CONTRACT_VIOLATION discipline in every driver | |
| Tier-2 never-prompt as spawn invariant; Tier-4 "don't expose the permission surface" | c10 cwd/sandbox **resolved ambient, removed from ledger**; lean flag set (`--permission-mode` posture / `approval_policy=never` + read-only sandbox) | The advisory independently re-derives the spec's c10 decision |
| Tier-2 per-backend known-answer conformance suite | The failure-bounded method itself + the pass's harness (`runs/.../harness/` = working conformance drivers) | Same anti-rot mechanism, same knife |
| Tier-3 spawn-scoped model/effort knobs | c1/c2 IN, measured spawn-scoped | Keep them spawn-scoped even though `turn/start` *can* take per-turn model/effort — smaller parity matrix (advisory) and that's how they were gated (ledger) |
| Tier-3 structured output | c7 IN (claude convention / codex enforced) | Advisory's "parity already decent" matches +12.5% gap, inside band |
| Tier-3 broadcast/gather, `list`, `render`; Tier-4 scheduling stays out | Pool/orchestration one layer up (parity analysis §5); `list` = cheap state-dir enumerate | No wire params → no ledger rows needed; composition of `turn` |
| Tier-4 fork/branch skepticism | Never a ledger candidate; under F0 its cross-engine state-fidelity divergence (true-branch vs replay) is precisely an F0 failure shape | If ever demanded, it enters as a candidate and very likely dies at F0 — both docs predict the same death |

## 2. Conflicts with the measured layer (parity results win — per the advisory's own rule)

### 2.1 The root conflict: codex backend assumption
The advisory is written against **cold `codex exec`+resume** ("codex threads are passive
files", thread-id statefile, exec-wrapper prototype). The pass measured warm `app-server`
at **918 ms vs 2150 ms cold (2.34×)** with verified pins → the codex adapter's fast surface
is **warm**. Consequences inside the advisory's text:
- *"stop: claude sessions are live processes; codex threads are passive files"* — under the
  warm adapter **both backends are live daemons with durable resume ids**; the asymmetry the
  advisory designs `stop`/`alive` around largely disappears (its turn-acceptability
  semantics remain correct and are adopted).
- The exec-era clauses stay TRUE **for the fallback surface** (exec+resume is retained as
  the no-daemon mode), so nothing is discarded — it is re-scoped.

### 2.2 Tier-4 "interrupt: ship timeout-plus-stop until a real need"
**Measured:** c8 is IN — claude `control_request` 0.5 ms ack, 6/6 clean; codex SIGINT 70 ms,
threads resumable 12/12 (the advisory's corrupt-thread fear was not observed, though the
check was behavioral — resume works, replies sane — not a deep state diff); codex warm
`turn/interrupt` 21.5 ms ack. **The advisory's skepticism survives in one specific form the
pass confirmed:** post-interrupt reply fidelity on warm codex is imperfect (re-answers the
aborted prompt ~2/6). **Resolution:** c8 ships, but with the measured obligation (treat the
first post-interrupt codex reply as suspect) — i.e. the advisory's "consumers route around
half-implemented cancel" prediction is treated as the failure mode the obligation exists to
prevent. Consumers that don't need cancel lose nothing (timeout+stop still works).

### 2.3 Tier-4 "streaming: parity nightmare, complexity-for-a-demo"
**Measured:** c6 is IN — and the "nightmare" was an artifact of the cold-backend assumption:
warm codex has token-level `item/agentMessage/delta` (TTFT 867 ms) symmetric with claude's
`content_block_delta` (1166 ms); the granularity cliff exists only on the exec fallback.
**The advisory's demand-side point is conceded** (no current consumer reads partials — a fact
the gates don't measure): **resolution = admit c6 as opt-in `--stream`, default off**, exactly
the parity-analysis backport shape. On the exec fallback, `--stream` degrades to
message-granular — documented, not papered over.

## 3. What the advisory adds to the ledger (filter b: new candidates/obligations)

1. **c11 `alive(handle)` — accepted as a new candidate, structural.** Cheap turn-acceptability
   predicate (v1 `status` exists; warm codex = daemon+thread known; cold codex = resumable
   thread exists). No completion-latency arm needed (control-class-adjacent); needs an F0
   state-table row + a conformance case ("alive ⇒ next `turn` succeeds; !alive ⇒ typed
   INIT_FAILED/TRANSPORT_ERROR, never a hang"). Prevents the blind-poll heuristics the
   advisory documents.
2. **c4 obligation (new, UNMEASURED): durable identity across daemon restart on warm codex.**
   The advisory's "handle must survive the caller's process exit" now has a warm-specific
   form: `thread/resume` into a fresh app-server process. Schema-confirmed, never exercised
   live. **This is the single highest-priority conformance case to run before the fork
   relies on the warm adapter** (alongside `thread/start.baseInstructions`, same status).
3. **Consumer semantics checklist** (advisory §consumer): spawn-time role injection ✓ (c3);
   transport-error-never-content ✓ (§1.1); **per-instance mailbox isolation for fan-out** —
   caller-side pattern, no wire change, but the fork's CLI should not preclude N concurrent
   `turn`s against distinct handles (the account-lane mutex is a consumer policy, not a
   transport rule).
4. **Tier discipline as build order** (the advisory's load-bearing structural claim, adopted
   verbatim): lock Tier-2 contracts before admitting anything Tier-4-shaped; every parameter
   arrives with its conformance case. This is the same knife as the failure-bounded method —
   the two documents are one discipline described from two sides.

## 4. Net directive for the fork build (merged)

- **Verbs:** `spawn/init` (model, effort, system — spawn-scoped), fused `turn` (timeout
  inside, typed errors, opt-in `--stream`, opt-in `--schema`), `alive`, `stop` (idempotent,
  turn-acceptability semantics), `resume` (durable ids both engines), `interrupt` (with the
  post-interrupt guard on codex). `list` as cheap sugar. Nothing else — no fork/branch, no
  permission passthrough, no scheduling.
- **Backends:** claude = v1 warm stream-json loop; codex = warm app-server (fast surface) +
  exec+resume (fallback). Adapter obligations: deliverable §6 + §3.2 above.
- **Before first fork commit:** exercise the two unmeasured warm-codex surfaces
  (`thread/start.baseInstructions`, `thread/resume` across daemon restart) as probe-grade
  conformance cases.
