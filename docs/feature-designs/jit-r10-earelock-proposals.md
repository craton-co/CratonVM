# Escape analysis and scalar replacement: directions worth pursuing

**Author:** round 10 wave 5, lane `earelock`.
**Scope:** `jit/src/x64/escape_analysis.rs` (`analyze_escapes`,
`plan_scalar_replacement`) and the gates around them.
**Standing:** every claim below is from reading the code in this worktree. This
lane was not permitted to build, run tests, or measure anything, so nothing here
carries a number that was not already written down by somebody who did measure
it. Where a proposal needs a measurement to be worth landing, it says so and
says which one.

The organising observation is this. The single-pass backend's scalar replacement
is guarded by **three independent** "no" answers, and every optimisation
proposal below is a way of turning one of them into a "maybe":

1. `analyze_escapes` is one linear pass, so **every CFG edge escapes everything
   live** (stack and locals) — §EA-2, §EA-4.
2. `x64/driver.rs` replaces the escape result with the **empty set** whenever
   `precise_exception_frames` is set, i.e. whenever any handler reads a
   non-parameter local — §EA-1.
3. `plan_scalar_replacement` admits an object only if `0 < num_fields <= 16` —
   §EA-5.

Each is individually sufficient to explain "scalar replacement did not fire
here", which is why fixing one at a time produces no measurable change and why
this round's monitor investigation (see
`docs/internal/fixed-bugs/r10-ea-single-pass-monitor-scalar-relock-is-unreachable-FIXED-20260922.md`)
concluded with "sound, cheap, and still not worth doing". **Any of these
proposals should be landed with a way to tell which of the three refused a given
site** — that is §EA-7, and it should probably go first.

---

## STATUS, 2026-09-22 — read this before any section below

Two of the three "no" answers above are gone, and one proposal on this page is
**unsound as written**. This block is the current reading; the sections below
are kept unedited because their reasoning is still the reasoning.

| § | State |
|---|---|
| EA-1 | **UNSOUND, not taken.** See the note under its heading. |
| EA-2 | **LANDED.** `bytecode_analysis::straight_line_gotos`, consulted by both walks. |
| EA-3 | Moot. The effect it proposed to get indirectly was got directly: `analyze_escapes` has exact `0xC2`/`0xC3` arms now. |
| EA-4 | Open, and still the right long-term shape. |
| EA-5 | Open, still the cheapest item here. |
| EA-6 | Open, and now MORE valuable — this change widens what the unverified backend emits. Filed separately. |
| EA-7 | Open, and still the right next investment. |
| EA-8 | Partly landed: the per-compile trace now carries `monitor_elided_pcs` and `monitor_at_pcs`, and it PRINTS (it never had — it was gated on a field assigned 140 lines later). The per-site refusal enum is not built. |

Gate 2 was removed by admitting scalar replacement **under**
`precise_exception_frames` rather than by narrowing the RBC.6 gate, which is the
other half of what the monitor page's step 1 allowed. The full account, with the
executed trace, is in
`docs/internal/fixed-bugs/r10-ea-single-pass-monitor-scalar-relock-is-unreachable-FIXED-20260922.md`.

---

## EA-1 — Seed the RBC.6 handler dataflow from the protected range, not from the parameters

> **UNSOUND AS WRITTEN. Do not implement this section. (2026-09-22)**
>
> The soundness argument below — "a slot assigned on every path into the range
> is a slot the *compiled frame* holds a real value for, which is precisely the
> condition the precise-exceptional-frame handoff already promises to publish" —
> conflates two different questions. The compiled frame does hold the value.
> But `local_handler_reads_unsafe_local` does not ask what the compiled frame
> holds; it asks whether `run_jit_callee_handler`'s **params-only**
> reconstruction can rebuild the handler's locals **from the incoming arguments
> alone**, and a slot first assigned inside the method body is not recoverable
> from arguments however definitely it was assigned.
>
> `handler_resume_requires_precise_locals`'s own doc says it in one sentence:
> *"for a `true` one, every local beyond the parameters resumes as 0/null,
> which is a silent wrong answer, not a crash."* Widening the seed set makes
> the predicate answer `false` for javac's monitor shape, which sends it down
> the params-only tier and resumes the handler with `<mon>` null — a
> `monitorexit` on a null instead of the rethrow the handler exists to perform.
> No crash, no test failure, a wrong answer.
>
> The problem statement below is still accurate and still worth solving; what
> was taken instead is the other half of the monitor page's step 1 — make
> scalar replacement work UNDER `precise_exception_frames` rather than narrowing
> the gate that sets it. That needed the reason-9 exceptional sinks to be able
> to rebuild a deleted object and re-acquire an elided lock, which they now can.
>
> Anything that DOES want to narrow this gate has to change the reconstruction,
> not the predicate: `route_jit_exception_through_method` would have to receive
> the locals from somewhere other than the incoming arguments. That is a
> different proposal and it does not exist yet.

**Problem.** `regalloc::handler_has_unsafe_local_read` starts its
"definitely assigned" dataflow *at the handler pc* with only `this` + declared
parameters marked safe. Anything the handler reads that was assigned before the
`try` is therefore reported unsafe, `lib.rs` sets `precise_exception_frames`, and
`x64/driver.rs` hands `plan_scalar_replacement` the empty set — scalar
replacement is off for the whole method.

This is not a corner case. It fires on **every** javac `synchronized` block,
because javac's mandatory monitor handler reads the synthetic monitor temporary:

```text
  astore <e> ; aload <mon> ; monitorexit ; aload <e> ; athrow
```

`<mon>` is allocated above every parameter slot, so it can never be in the seed
set. `jit/tests/r10_earelock_synchronized_scalar_gates.rs` pins exactly this,
with a one-byte control. `jit/src/lib.rs` independently names Tomcat's
`StringCache.toString` as a hot method refused for the same reason.

**Proposal.** The seed set for a handler at `H` should be the **intersection,
over every pc `p` in every protected range that names `H` as its handler, of the
"definitely assigned at `p`" set** — not the parameter set. That is the real
question ("what is guaranteed assigned when control can reach this handler?"),
and it is computable with the machinery already present: run the same
`handler_has_unsafe_local_read` forward must-dataflow once from `entry_pc = 0`
with the parameter seed, record its per-pc `safe_at` map, then intersect over
the range.

`<mon>` is assigned by the `astore <mon>` that immediately precedes the
`monitorenter`, which precedes the protected range by construction — javac
cannot emit it any other way, because the range must start after the lock is
taken. So the intersection contains `<mon>` and the gate opens.

**Why this is sound-preserving.** The predicate answers "can the interpreter
rebuild this handler's frame from the incoming arguments alone?". A slot
assigned on every path into the range is a slot the *compiled frame* holds a
real value for, which is precisely the condition the precise-exceptional-frame
handoff already promises to publish. This proposal makes the predicate less
conservative in the direction its own doc comment says an earlier version was
wrong in — it documents a false-reject on
`org.apache.catalina.connector.Response.toAbsolute()` from over-scanning, and
this is the same class of false reject from under-seeding.

**Risk and how to bound it.** `local_handler_reads_unsafe_local` gates more than
scalar replacement — it decides whether a method reaches the optimizing tier at
all, and `precise_handler_frames_enabled`'s doc lists four defects that a
previous relaxation of this area exposed (all fixed; all named). So: land it
behind its own flag, default off, and measure the json-smart round-trip probe at
`docs/known-issues/repros/jsonsmart/` with `-Xmx64m` for ~20,000 iterations —
that is the probe those four defects were found with, and it is the right
acceptance test for anything that widens this population.

**Payoff to measure.** Method count admitted to the optimizing tier, before and
after, on the Tomcat and json-smart suites; and `StringCache.toString`
specifically, which is documented as sitting "in the middle of a 600M-call hot
chain whose neighbours both compile".

**Effort.** Medium. One extra dataflow pass in `jit/src/regalloc.rs`, one seed
change in `jit/src/lib.rs`, no change to the backends.

---

## EA-2 — Let `analyze_escapes` walk through a `goto` that is the sole ordinary entry to its target

**Problem.** `analyze_escapes`' control-transfer barrier (`0x99..=0xa9`, plus
`0xbf`, `0xc6..0xc9`) escapes every object on the stack **and in every local**,
then clears all provenance. The comment explaining it is correct and the
miscompile it fixed is real (bc `org.bouncycastle.math.ec`, a SEGV in the young-
gen scavenge copy loop). But it is blunt: it treats an unconditional forward
`goto` — which has exactly one successor and no merge at all — the same as a
conditional branch into a merge point.

The shape this costs is common. javac emits it around every `try`/`finally` and
every `synchronized` block:

```text
  18: monitorexit
  19: goto 27          <-- barrier fires here: escapes f out of locals 1 and 2
  22: astore_3         <-- reachable only through the exception table
  ...
  26: athrow
  27: return
```

`r9_ea_tests::an_exact_non_escaping_monitor_arm_would_not_rescue_the_javac_synchronized_shape`
pins that this `goto`, on its own, is enough to lose the receiver.

**Proposal.** Treat a `goto`/`goto_w` at `p` with target `t` as a straight-line
continuation — keep `abs_stack` and `local_origin`, set `pc = t` — when all of:

1. `t > p` (forward only: a back edge is a real loop merge);
2. `t` has exactly one *ordinary* predecessor, namely `p` itself. Computable
   from `bytecode_analysis::branch_target_map` extended to count sources, or
   from `InsnCfg`'s predecessor lists;
3. every instruction in `[p + 3, t)` is reachable only through the exception
   table — i.e. the skipped region is exclusively handler code. `bytecode_analysis`
   already has the reachability primitive ("Pcs reachable from the method entry
   … along ORDINARY control flow", with handlers as roots only when passed in);
4. the method is not one whose handlers actually run in this compiled frame.
   Scalar replacement already requires that (`precise_exception_frames` off, and
   `local_handler_reads_unsafe_local` false), so this condition is free here —
   but state it explicitly, because condition 3 skips code that an exception
   *could* otherwise execute, and the whole argument is that it cannot.

Conditions 1-3 are decidable before the walk starts; compute a
`goto_is_straight_line: Vec<bool>` alongside `branch_targets` and consult it in
the barrier arm. The walk stays linear and single-pass.

**Why not just "keep locals at a goto".** Because the skipped region may contain
`astore`s (the handler's `astore <e>`), and the linear walk would then model the
target with state from a path that could also have been reached through the
handler. Condition 3 plus condition 4 is what rules that out.

**Interaction.** This is step 2 of the monitor page's three-step fix list. On its
own it does nothing for javac `synchronized` blocks, because EA-1's gate closes
first. It does stand alone for `try`/`finally` shapes in methods whose handlers
read only parameters — those are admitted today and lose their allocations at
the `goto` for no reason.

**Effort.** Small-to-medium, and it is the highest-risk item here in proportion
to its size, because it is a direct loosening of the model behind a named SEGV.
It should land with the differential harness of §EA-7 already in place, not
before.

---

## EA-3 — Give the single-pass backend the IR tier's documented two-phase protocol

**Observation.** The IR-based `jit/src/escape_analysis.rs` already handles
"locked but otherwise local" end to end, and its doc comment (~line 1700)
describes the protocol: run EA, apply the lock elision it offers (turning the
monitor ops into `Op::Dead`), then **re-run EA on the mutated graph** so the now-
dead monitor no longer blocks scalar replacement. The gate there is the separate
identity-observation mechanism (`find_identity_observations`,
`ScalarRefusal::MonitorOperation`), not an escape edge — `build_connection_graph`
gives `Op::MonitorEnter`/`MonitorExit` no edge at all.

The single-pass backend has no analogous loop. `x64/driver.rs` already re-runs
`analyze_escapes` once (with resolved `invokespecial` shapes), so the shape of a
second iteration exists; what is missing is a mutation between the two runs.

**Proposal.** After the first `analyze_escapes`, compute the set of
`monitorenter`/`monitorexit` PCs whose receiver provenance is a `new` that the
pass would otherwise admit, rewrite them to `nop` in a scratch copy of the
bytecode, and re-run. This gets the effect of an exact `0xC2`/`0xC3` arm without
adding one, and — importantly — keeps `analyze_escapes` itself unchanged, so
every regression that function's comments name stays covered by the unmutated
first run.

**Caveat, and it is the decisive one.** This is still subject to EA-1's gate and
EA-2's barrier. Sequenced after both, it is a small addition; sequenced before
either, it is provably a no-op on javac output. Do not land it first.

---

## EA-4 — Replace the linear pass with a worklist over the CFG the backend already builds

**The structural point.** Every "conservative" behaviour in `analyze_escapes` —
the branch barrier, the `wide` barrier, the `athrow` barrier, the surplus-depth
catch-all — exists because the pass has no merge operator. `jit/src/regalloc.rs`
already builds a basic-block CFG with exception edges
(`build_cfg_with_handlers`) and already runs a bitset dataflow to fixpoint over
it (`solve_liveness`), for this same backend, on this same bytecode.

**Proposal.** Recast `analyze_escapes` as a forward must-analysis over that CFG:
per-block entry state `(abs_stack, local_origin)`, merged at each join by "if the
two incoming provenances differ, escape both and record `None`". Iterate to
fixpoint (monotone: the escaped set only grows, provenance only weakens).
Straight-line regions behave exactly as today; a diamond whose two arms agree —
`if (c) f.x = 1; else f.x = 2;` — keeps its scalar for the first time.

**Why this is the right long-term shape and the wrong short-term one.** It
subsumes EA-2 and most of EA-3, and it removes the class of argument that makes
every small change here expensive ("is this loosening sound?" becomes "is the
merge operator sound?", asked once). But it replaces a function whose current
behaviour is pinned by three named production miscompiles, and this lane cannot
measure. It is a whole round's work with a full differential corpus run, not a
wave's.

**Sequencing.** Do §EA-7 first, then this, then delete EA-2 from the plan.

---

## EA-5 — Admit zero-field objects (scalar replacement as pure deletion)

**Problem.** `plan_scalar_replacement` only builds an entry when
`num_fields > 0 && num_fields <= 16` (`escape_analysis.rs`, the `sorted_pcs`
loop). A class with **no** instance fields is therefore never scalar-replaced —
even though it is the easiest possible case: there is nothing to put in the
frame, and the optimisation degenerates to deleting the allocation and its
`<init>()V` outright.

The population is not exotic:

* `new Object()` as a local lock or a sentinel — the canonical
  `synchronized (new Object())` / `Object token = new Object();` shape;
* marker and tag types, empty exception subclasses constructed on a path that
  discards them, `new Object[]`-free iterator sentinels;
* anything the inliner leaves behind as a receiver whose fields all got
  constant-folded away.

**Proposal.** Allow `num_fields == 0`, with `field_base_offset` unused and
`total_slots` contribution zero. Two things must be checked rather than assumed:

* `used_objects` currently requires at least one field op **or** an init skip. A
  zero-field object has no field ops, so it survives only via its
  `init_skip_owner` entry — which means the `<init>()V` must have been proved
  empty (`elidable_init_pcs`), which is exactly the right condition. Confirm the
  `0xbb` codegen arm handles `num_fields == 0` (its zero-init loop simply does
  not iterate; the dummy-zero push is unchanged).
* `FrameValue::VirtualObject` with zero fields must round-trip through
  `deopt_materialize` — a materialised zero-field shell still needs its class id
  and header.

**Payoff.** Unmeasured, and plausibly small in allocation *bytes* but not in
*count*; a TLAB bump plus a header write plus a post-init helper call per site is
not nothing on an allocation-heavy loop. Measure with the existing
`object_allocation/1000` probe named in `op_object.rs`'s HIGH-6 comment.

**Effort.** Small. This is the cheapest item on this page and the only one that
does not touch a soundness argument.

---

## EA-6 — Run `DeoptVerifier` on the single-pass backend's metadata

Filed in full as
`docs/internal/fixed-bugs/r10-earelock-singlepass-deopt-metadata-is-never-verified-FIXED-20260922.md`.
Summarised here because it is a prerequisite for anything in §EA-1..EA-4 that
widens what scalar replacement emits: the optimizing tier verifies its deopt
metadata at install time and the single-pass backend does not, so a producer bug
in `x64/deopt_stubs.rs` ships silently. Round 10 found one such bug by reading
(a `VirtualObjectRef` in the monitor lane with no defining `VirtualObject` in the
locals lane) that the verifier has a dedicated error variant for.

One pass over a small vector, per compile, in `x64/driver.rs` next to
`can_deopt_resume`; on violations, drop the deopt points rather than failing the
compile.

---

## EA-7 — A differential harness for the two escape analyses (do this first)

**Problem.** There are two escape analyses in this repo — the single-pass
`x64/escape_analysis.rs::analyze_escapes` and the IR connection graph in
`escape_analysis.rs::build_connection_graph` — and this round's finding is
literally that they disagree about `monitorenter`. They also disagree about
`checkcast`, `pop2`, the `dup_x*` family and every `Xaload`, in the same
direction (the single pass escapes; the IR pass models). Nothing tests the
relationship.

Worse, the *same file's* two walks (`analyze_escapes` and
`plan_scalar_replacement`) are a matched pair whose agreement is load-bearing —
the `poisoned` set's doc comment is four paragraphs about a time they drifted —
and their agreement is likewise tested only by example.

**Proposal.** A property test in `jit/tests/` over generated straight-line
bytecode (the corpus that matters: `new`/`dup`/`<init>`/`astore`/`aload`/
`getfield`/`putfield`/arith/loads, plus one of each barrier opcode), asserting
the two invariants that must hold:

1. **Ordering.** For every generated method, `analyze_escapes`' confirmed
   non-escaping set is a subset of what the IR connection graph would admit.
   A violation means the single pass is the *looser* of the two, which is the
   unsound direction.
2. **Pairing.** For every `new_pc` that `analyze_escapes` admits,
   `plan_scalar_replacement` either scalar-replaces it or poisons it — never
   "keeps the allocation and also maps some of its field ops". This is the exact
   invariant the `poisoned` machinery exists to maintain, and it is currently
   maintained by hand.

Plus one cheap structural assertion that would have caught this round's monitor
finding directly: **every opcode that reaches `analyze_escapes`' catch-all
should be listed in a test's explicit `EXPECTED_UNMODELLED` set**, so adding an
opcode to the catch-all (or, as happened here, never noticing one is there) is a
deliberate edit to a named list rather than an absence.

**Why first.** Every other proposal on this page is a loosening of a model whose
current behaviour is pinned by production miscompiles. §EA-7 is the only item
that makes the next loosening cheaper to argue about, and it is also the only one
that costs nothing if it turns out the loosening is not worth landing.

**Effort.** Medium. `jit/tests/differential.rs` and
`jit/tests/backend_parity_analysis.rs` are the precedents for the harness shape.

---

## EA-8 — Make "why was this site not optimised?" answerable per site

Two of this round's three filed defects are the same shape: a counter or a flag
that reads zero forever, indistinguishable from "it never happens"
(`has_elided_monitor`, the five `CHECKCAST_INLINE_*` statics). The escape
analysis has the same hole in a worse place — three independent gates, no way to
ask which one refused.

**Proposal.** A per-compile `ScalarRefusal`-style verdict for the single-pass
backend, matching what the IR tier already has (`ScalarRefusal::MonitorOperation`
et al.) and what the admission chain already does for tier selection (`lib.rs`'s
lazily-built `verdict` string, which is read by three consumers). One enum with
the four answers that exist today — `Baseline`, `PreciseExceptionFrames`,
`Escaped { at_pc, opcode }`, `Poisoned { at_pc }`, `FieldCountOutOfRange` —
surfaced through the same `CRATONVM_DBG_SCALAR_DEOPT` line that already prints
`scalar_replaced=` and `elided_monitor=`.

This is what turns §EA-1..§EA-5 from "land it and hope the benchmark moves" into
"land it and watch a refusal count go to zero". Given that all three gates
currently answer "no" for overlapping populations, it is close to a prerequisite
for measuring any of them.

**Effort.** Small. No new machinery; the debug line and the flag already exist.
