# Round 10 `ea` lane — proposals (escape analysis / regalloc / x64 encoding)

Lane scope: `jit/src/escape_analysis.rs`, `jit/src/x64/escape_analysis.rs`,
`jit/src/ea_ir_bridge.rs`, `jit/src/regalloc.rs`, `jit/src/x64/emit.rs`,
`jit/src/x64/disp.rs`, `jit/src/x64/reg_encoding.rs`.

Two concrete proposals, each with a mechanism and a cost estimate, plus one
smaller follow-up noted at the end.

## 1. Make `analyze_escapes` model `monitorenter`/`monitorexit` precisely, so the
   existing "Phase C" relock machinery becomes reachable

**Motivation.** Filed as
`docs/internal/fixed-bugs/r10-ea-single-pass-monitor-scalar-relock-is-unreachable-FIXED-20260922.md`
this round: `jit/src/x64/escape_analysis.rs`'s `plan_scalar_replacement` already
contains a complete, tested feature for scalar-replacing an object that is also
the target of a `synchronized` block and relocking it on deopt
(`mon_depth`/`monitor_at`/`monitor_scalar_ops`, consumed at
`jit/src/x64/op_object.rs:918`). It can never fire because the upstream
`analyze_escapes` treats `monitorenter`/`monitorexit` as unmodelled opcodes and
unconditionally escapes their receiver via its catch-all arm. This is the
single most common shape that defeats scalar replacement in real code: a
short-lived helper object (a local `StringBuilder`-shaped accumulator, a
lock object used only to guard a few lines) that is synchronized on but never
otherwise escapes.

**Mechanism.**

1. Add explicit `0xC2 => { .. }` / `0xC3 => { .. }` arms to `analyze_escapes`,
   modelled on the arms already added for primitive stores and arithmetic in
   the same function (see the `0x36..=0x39` and `0x60..=0x73` comments for the
   documentation style this file expects). Each arm pops exactly one abstract
   stack slot (the receiver) and does **not** call `escaped.insert` on it —
   mirroring the IR-based `escape_analysis.rs::build_connection_graph`, whose
   `match` has no arm for `Op::MonitorEnter`/`Op::MonitorExit` at all (they fall
   to the silent `_ => {}`, contributing no escape edge).
2. `plan_scalar_replacement`'s existing Phase C code needs no changes — it
   already correctly tracks recursion depth, per-PC monitor snapshots, and
   prunes `monitor_scalar_ops` to objects that survive every other check
   (`jit/src/x64/escape_analysis.rs:1310-1345`). The fix is entirely on the
   `analyze_escapes` side of the join.
3. Add a same-file unit test (next to `s32_phase_c_plan_tracks_scalar_monitors`
   in `jit/src/x64/tests.rs`, owned by a different lane) that runs
   `analyze_escapes` — not a hand-built `non_escaping` set — on the exact
   bytecode that test already uses, and asserts the `new` pc is non-escaping.
   That closes the gap this round's finding identifies: today nothing tests the
   *join* between the two passes, only each one in isolation.
4. Run the differential/OSS corpus this file's own comments name as the
   regression surface for exactly this kind of change (bc `math.ec`, a kafka
   cache-eviction benchmark, Spring's `ResourceDatabasePopulator`), plus a new
   micro-benchmark: a hot loop calling a method that allocates a local lock
   object, synchronizes on it, and lets it die — measuring both correctness
   (no regression on the named corpus) and the win (no allocation, no real
   futex/thin-lock traffic).

**Cost.** Small code change (two match arms, each a handful of lines), but
correctness-sensitive: this exact function has caused three documented
production miscompiles when its stack/provenance model was loosened without a
build+measure cycle in the same session. Needs to be done by a lane that can
build and run the JIT, with the differential corpus in step 4 as a hard
gate before landing. Expected win: unlocks scalar replacement + lock elision
for every synchronized-and-otherwise-local allocation the single-pass tier
compiles — a shape common enough (thread-safe builder idioms, defensive local
locking) that it is worth the validation cost.

## 2. A standing cross-check between the x64 single-pass and IR escape analyses

**Motivation.** This round's brief asks explicitly whether the two
implementations "agree on the same program," and whether a disagreement "fails
safe in both directions." Today that question can only be answered by manual
reading (as this lane did for the monitor case above) — there is no
machine-checked property tying the two together, even though both walk the
same bytecode for the same method whenever the optimizing tier is available for
it (`jit/src/lib.rs` runs the single-pass tier first, then may promote to the
IR tier).

**Mechanism.** Add a debug/CI-only "shadow comparison" mode, gated behind a
runtime flag (matching the existing `CRATONVM_DBG_SCALAR_DEOPT` /
`CRATONVM_DISABLE_SCALAR_REPLACEMENT` pattern already in
`jit/src/x64/driver.rs`), that — only when both the single-pass escape result
(`analyze_escapes`'s `non_escaping_new`, keyed by bytecode pc) and the IR-based
result (`escape_analysis::analyze_escapes` over the bridged graph, keyed back to
bytecode pc via the `New`/`NewArray` node's originating bci, which
`ea_ir_bridge.rs` already has to track for other purposes) are available for the
same method — diffs the two non-escaping sets and logs (never bails or panics;
this is purely observational) any allocation site where:

* the single-pass tier calls an allocation non-escaping but the IR tier does
  not (a **soundness alarm**: if this is ever seen for a real method, one of
  the two implementations is unsound and this round's whole "fail safe in both
  directions" question has a counter-example worth investigating immediately);
* the IR tier calls an allocation non-escaping but the single-pass tier does
  not (an expected, benign gap the IR tier's use of context the single-pass
  walk does not have — bridged type info, memory-token ordering, etc.).

The comparison itself is cheap (an `FxHashSet<usize>` intersection/difference
over allocation pcs already computed for other reasons), so the overhead when
enabled is negligible relative to the two analyses it is comparing, and it is a
complete no-op when the flag is off (checked once, matching the existing
flag-gated debug paths in this file).

**Cost.** Moderate: needs the bci-recovery plumbing to map an IR `Op::New` node
back to the bytecode pc `analyze_escapes` uses as its key (some of this may
already exist for deopt/bailout reporting — worth checking `ea_ir_bridge.rs`
and `ir_lower.rs` for an existing bci table before building a new one). Payoff
is a standing, cheap, always-available answer to exactly the question this
round's `ea` lanes are asked to answer once by hand — future rounds (and CI, if
wired into the existing differential-corpus run) get it for free on every
method compiled by both tiers, rather than only on the methods a reviewer
happens to read.

## Smaller follow-up (not a full proposal)

`jit/src/regalloc.rs`'s own doc comment on `SafepointPublishPlan`
(~line 1374) names a "long-standing `reg_oops` bitmap TODO" — a register-level
GC root map that would let a reference-typed local stay in a callee-saved
register across a safepoint without the pre-call publish-to-frame-slot copy
this round confirmed is still the only mechanism (`jit/src/regalloc.rs`'s
`verify_allocation` still refuses a reference held in a register across a
safepoint whenever `!model.refs_may_cross_safepoints`, which is the x64/ARM64
backends' current setting). This is already tracked in-repo (the TODO comment
plus the cross-reference to `jit-regalloc-and-deopt.md`) and this lane did not
find anything to add beyond confirming it is still accurate as of this round —
noted here only so a future round proposing register-level oop maps has a
pointer to the exact soundness invariant (`verify_allocation`'s safepoint-crossing
check) it would need to relax, and the exact per-call cost
(`SafepointPublishPlan::publish_at_bci`) it would remove.
