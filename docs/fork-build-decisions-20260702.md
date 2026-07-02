# Fork Build — Locked Design Decisions (grill session 2026-07-02)

**Status:** FROZEN — this is the decision record produced by a /grill-us session
(Fable interviewer, Codex consultant) over the merged directive of
`docs/advisory-reconciliation-20260702.md` §4, grounded in
`docs/lean-parity-measurement-pass-20260701.md`. Where this document and those
inputs disagree, this document wins; where it is silent, the reconciliation §4
directive stands. Each decision below was frozen, independently consulted, and
chosen by the owner.

---

## D1. Warm is a cache, never the identity owner

Durable session identity on codex is owned by the **thread file + `exec resume`**
(measured, verified). The warm app-server is a latency/cache layer over that
substrate. `resume` across caller or daemon restart may transparently demote to
the exec fallback; warm speed is only guaranteed within one daemon generation.
Probe failure is a performance regression, never an identity-model collapse.

- The two keystone probes (`thread/start.baseInstructions`, `thread/resume`
  into a fresh app-server) run **before design freeze**, not merely "before
  first commit."
- The restart probe must verify **state equivalence** across the warm→cold
  seam — transcript identity, instructions, cwd, session metadata — not just
  that resume returns.
- Explicit rule required in the adapter design: no *required* state may live
  warm-only (nothing trapped in the daemon).
- `baseInstructions` probe failure contingency is free: AGENTS.md-in-cwd is
  verified and persists across resume.

## D2. Per-handle app-server children, pipe daemon as sole supervisor

One app-server child **per handle**, supervised by the pipe daemon. No systemd
units, no split ownership. Verified: this is the exact topology the 918 ms P50
was measured on (`harness/codex-warm-driver.mjs` — one child, one
`thread/start` per conversation).

- Crash semantics: child exit detected by the daemon → handle demotes to cold
  `exec resume` with a typed, logged event — never a hang, never content —
  and rewarms lazily on the next turn. Demotion does not flip `alive`.
- ~600 ms one-time warm init per handle is in the measured budget.
- Multiplexing threads onto a shared app-server per account lane is a **later
  optimization gated on a new probe**: concurrent turns on distinct threads
  without head-of-line blocking. Do not assume it.

## D3. Conformance-gated version drift (expanded scope)

No hard pin of the codex binary; drift is detected mechanically.

- Handle records codex binary version + resolved model per spawn (extends the
  already-mandatory `config/read`).
- The measurement-harness drivers grow into a **first-class in-binary
  conformance suite** (`<fork> conformance [--fast]`) — product code with
  fixtures, timing bounds, and self-judging pass/fail, not capture scripts.
  (Audited: the existing probes check the right deterministic protocol facts —
  rollout ground truth, `config/read` tier, sentinel-by-threadId, resolved-model
  echo — but must be rewritten as assertions.)
- On first spawn after a codex version change, the daemon auto-runs the fast
  subset. Warm-only failures → typed demotion to exec surface; both surfaces
  failing → INIT_FAILED with the conformance diff as diagnostic. Never silent
  wrong behavior.
- **Owner-expanded scope:** the version-change suite is deliberately larger
  than protocol drift. A codex update could silently move functionality to a
  different billing cycle, so every version-change run bundles three assertion
  classes: (1) protocol facts, (2) speed regression, (3) **auth/billing-posture
  acceptance** — subscription OAuth accepted, tier=standard, credits off —
  asserted as *effects*, not accepted flags (the `base_instructions`
  accepted-but-ignored death is the template failure).
- Optional config: pinned codex binary path for hard-stability users; not the
  default on a self-updating box.

## D4. Structural `alive`, lazy-ready, honestly weakened promise

No probe turns. Readiness and liveness are structural predicates:

- codex-warm ready = `initialize` + `thread/start` returned. codex-demoted
  ready = thread file exists and parses. claude ready = lazy-ready (process
  spawned, stream open; **never** wait for the init event — measured 14 s
  median late); the first real send is the true test, failure mapped to typed
  INIT_FAILED.
- `alive(handle)` = structural turn-acceptability: supervisor sees the child
  (or a resumable thread artifact), session/thread id known, no fatal error
  recorded.
- The c11 promise is restated: **alive=true ⇒ a turn attempt is safe and its
  outcome is typed (success or typed failure — never a hang, never a silently
  new session); alive=false ⇒ turn refuses fast with a typed error.** The
  original "alive ⇒ next turn succeeds" is unkeepable without spending a turn.
- Documentation obligation: `alive` is "safe to attempt," not a success
  guarantee. The no-hang guarantee is *enforced* in `turn`'s internal timeout
  and stream-failure→typed-error mapping — that is where the conformance case
  bites.
- Opt-in `--probe` (real turn) is demand-gated future surface, not v1.

## D5. `system` promises delivery invariants, not semantic uniformity

The cross-engine contract for `spawn(system)` is exactly the three verified
invariants: (1) in effect from the first turn; (2) out-of-band — zero injected
conversation turns; (3) persists across `resume`.

- Replace-vs-layer is **documented engine-native divergence**: claude =
  replace (`--system-prompt`; also suppresses the haiku side-call), codex =
  layer (`baseInstructions` if the probe passes; AGENTS.md fallback).
- Do not normalize claude to layering via `--append-system-prompt` (unmeasured
  channel; weaker role injection). Strict-symmetry layering is a future config
  toggle with its own conformance case, if ever demanded.
- Owner rationale, recorded: the project is **one tool, one state, one
  interface, many agents**. Agents and harnesses are already deeply divergent;
  by the time text reaches the model there are many instruction layers. The
  interface does not pretend otherwise.
- Verified consumer evidence: bsoup's proven seam needs delivery-at-spawn and
  turn-one effect; it tolerates asymmetric precedence.

## D6. Post-interrupt: unconditional suspect flag + spawn-scoped fence opt-in

Measured hazard (interpretation corrected and pinned): after `turn/interrupt`
on warm codex, ~2/6 of the time the **next** turn's reply re-answers the
aborted prompt, arriving under the **new** turnId. This is thread-semantic
residue — explicitly NOT a streaming race (excluded by turnId analysis) and
NOT a receipt-side artifact (the interrupted turn already ends typed). Claude
never exhibits it. Interrupt ack itself is fast and reliable (21.5 ms, 6/6).

- The first `turn` after an `interrupt` on a codex handle returns its reply
  verbatim **plus** a typed `post_interrupt_suspect: true` in the result
  envelope. The transport never judges content — typed warning is never a
  content judgment (same knife as typed-failure-never-content).
- `spawn` accepts `fence_on_interrupt`: when set, the adapter injects a fence
  turn after each interrupt (drain residue; costs ~1 s + a quota turn) so the
  next real reply is clean by construction.
- **RESOLVED (gate-plan session, same date): `fence_on_interrupt` defaults ON.**
  The transport's consumer population is unattended agents (bsoup, arbiter,
  workflow runners) that cannot "display a warning," so the default assumes
  unattended; attended/UI consumers opt out per spawn. No separate lane
  concept is introduced. The default-on fence path is the default conformance
  case; the opt-out path carries the suspect-flag case.
- No adapter-side relevance verification, ever (semantic judgment inside the
  transport is forbidden).
- Conformance cases: interrupt → next turn flagged; following turn unflagged;
  claude handles never flagged; fence mode → unflagged clean reply.

---

## Public contract, negative form (adopted from final consult)

The fork's envelope guarantees, stated as prohibitions:

1. **No silent new session** — a turn never implicitly creates or forks
   identity.
2. **No content-as-error, no error-as-content** — transport failures are typed;
   typed warnings are never content judgments.
3. **No untyped hang** — every turn resolves within its timeout to success or
   a typed failure.
4. **No warm-only identity** — nothing required for resume lives only in a
   daemon process.
5. **No silent demotion** — every fast-path→fallback transition emits a typed
   event and marks handle metadata.

## Consolidated pre-build gate (updated probe/task list)

1. `thread/start.baseInstructions` probe (effect-asserted in rollout).
2. `thread/resume` into fresh app-server, with **state-equivalence** diff
   across the warm→cold seam (D1).
3. Warm/cold resume-agreement invariant defined precisely: thread id,
   cwd/instructions, transcript continuity, failure typing (final consult §4).
4. Conformance suite rewritten as self-judging product code (D3), including
   billing-posture assertions.
5. Deferred, pre-multiplex only: app-server concurrent-thread probe (D2).

Residual risk, named: not architecture — **rot in the thin boundary code**.
The conformance suite and the typed envelope are product surfaces, not test
scaffolding.
