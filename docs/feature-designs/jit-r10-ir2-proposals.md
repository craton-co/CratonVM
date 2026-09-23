# IR core / optimizer proposals — round 10, lane `ir2`

**Status:** PROPOSALS ONLY. Written after a hard review of `jit/src/ir.rs`,
`ir_verify.rs`, `ir_evidence.rs`, `ir_optimize.rs`, `ir_check_elim.rs` and
`ir_schedule.rs`, focused on constant-fold edges, box/unbox handling, pass
reachability, CSE/GVN/code-motion safety, verifier vacuity and unread
evidence. Nothing below was built or measured; each item states its own cost.
No correctness bug was found in this pass beyond the four already fixed and
recorded in the round's brief (`ir.rs` type-of-dead-node and unbounded
`ref_origin_at` recursion, `ir_check_elim.rs`'s unreachable-block dominance,
`ir_optimize.rs`'s hardcoded unroller phi slots) — all four are present and
intact in this tree. See the lane's final report for the sweep that did not
turn up a fifth.

## 1. `try_fold` never reaches `Op::ScalarIntrinsic`, so a constant argument to
   an intrinsic never folds

**Where:** `jit/src/ir_optimize.rs`, `try_fold` (the `match &node.op` at
line 858) and `try_simplify` (line 1118). Neither has an arm for
`Op::ScalarIntrinsic(_)`; the op falls through to each function's `_ => None`.
`Op::ScalarIntrinsic` families are enumerated in `jit/src/ir.rs`'s `ScalarOp`
(`AbsI`/`AbsL`/`AbsF`/`AbsD`, `NlzI`/`NlzL`/`NtzI`/`NtzL`,
`ReverseBytesI`/`ReverseBytesL`, `LowestOneBitI`/`HighestOneBitI` and their
`long` twins, `RotateLeftI`/`RotateRightI` and their `long` twins,
`CompareI`/`CompareL`) — every one a **total, pure function of its operands**,
which is exactly `is_pure()`'s bar (`ir.rs` line 1142 lists
`Op::ScalarIntrinsic(_)` there, so GVN already treats it as safe to
deduplicate).

**What is true today.** `Math.abs(-5)` on a folded constant, or
`Integer.numberOfLeadingZeros(0)` on one, survives the scalar-cleanup
fixpoint as a live node computing a value `try_fold`'s own sibling functions
could answer at compile time. GVN will still merge two *identical* such
calls, but it cannot turn one into a `Const`, so it cannot feed
`algebraic_simplify`, the unroller's trip-count simulation
(`analyze_counted_loop`'s `const_i64`, which only recognises `Op::Const`), or
a downstream branch fold. A counted loop whose trip bound is
`Math.abs(-8)` — a shape a constant-folder upstream of javac would never
leave, but one this tier's own unrolling of an outer loop can produce by
materialising an intrinsic call over a now-constant induction value — is
reported `NotCounted::Shape` today for a reason that has nothing to do with
the loop's real shape.

**Proposal.** Add a `Op::ScalarIntrinsic(op)` arm to `try_fold` that pattern
matches each `ScalarOp` variant against `const_value` on its one or two
inputs and computes the JLS-defined answer directly (the module comments in
`ir.rs` already spell out every edge: `AbsI`/`AbsL` at `MIN_VALUE`,
`NlzI`/`NlzL`/`NtzI`/`NtzL` at zero, `RotateLeftI`/`RotateRightI`'s count
masked to 5/6 bits including a negative count, `LowestOneBitI`/
`HighestOneBitI` at zero). `AbsF`/`AbsD` need `ConstF`-typed inputs, which
`try_fold`'s `i64`-only contract does not carry — either give them their own
narrow arm operating on bit patterns (`AND` with the sign mask, matching
`ScalarOp::AbsF`/`AbsD`'s own lowering, which the module comment states is
exactly one instruction and NaN/`-0.0` safe by construction) or leave the two
FP variants out of this proposal's first cut and fold only the eleven
integer/long families.

**Why it is worth doing.** Every one of these intrinsics is cheap to fold and
several (`NlzI`, `HighestOneBitI`) are exactly the shape a strength-reduced
bit-manipulation loop bound takes after an earlier round of constant
propagation. The cost of NOT folding is a live call-shaped node blocking
`analyze_counted_loop`, the range pass's `Const` arm in `proven_non_negative`,
and any later `algebraic_simplify` rule keyed on `is_const_val`.

**Cost.** Small and mechanical for the eleven integer/long families — each is
a direct transcription of the JLS rule already documented next to the
`ScalarOp` variant in `ir.rs`. The two FP variants are a few more lines given
a bit-pattern arm. No new test infrastructure: `jit/src/ir_optimize.rs`
already has a `fold_binop`/`fold_unop` test harness (see the `#[cfg(test)]`
`mod tests` around line 7783) that a `fold_scalar_intrinsic` harness could
mirror directly.

## 2. The optimizing tier has no boxing-side (`valueOf`) recognition at all,
   so an identity-sensitive box optimization has nowhere to attach

**Where:** `jit/src/ir.rs`'s unboxing machinery (`UnboxOp`,
`try_ir_unbox_intrinsic`, `unbox_offsets`, `try_emit_unbox_intrinsic`) lowers
`Integer.intValue()`/`Long.longValue()`/etc. as a guarded inline field read.
There is no equivalent for the boxing half (`Integer.valueOf(int)` and
friends) anywhere in the six files this lane owns — `grep -rn
"IntegerCache\|valueOf" jit/src/ir*.rs` finds only a test-fixture triple that
asserts the *unbox* recognizer correctly **declines** `valueOf` (see
`ir.rs`'s `the_unbox_recognizer_matches_only_its_declared_triples`, which
calls out the boxing half explicitly: *"The BOXING half: `valueOf` allocates
or reads a cache, and has no [recognizer]"*).

**What is true today.** Because nothing in the optimizing tier models
`valueOf`, there is also nothing that could fold `new Integer(5) ==
Integer.valueOf(5)` incorrectly, or merge two `Integer.valueOf(100)` call
sites into one shared cached object — the identity hazards named in this
round's FOCUS list for box/unbox simply have no code path to go wrong in yet.
GVN cannot CSE two `valueOf` calls (they are ordinary `Op::Call`s, not pure),
and no pass in `ir_check_elim.rs` or `ir_optimize.rs` reasons about the
`Integer.valueOf` cache range (`-128..=127`) at all.

**Proposal.** If a future round wants a `valueOf` fast path (as a scalar
intrinsic that reads the cache array directly for a compile-time-constant
argument in range, and falls back to the real call otherwise, mirroring how
`try_emit_unbox_intrinsic` special-cases the guarded inline read), it should
land as a NEW recognizer parallel to `try_ir_unbox_intrinsic` rather than by
widening the existing one — the existing docstring is explicit that the two
"have no argument in common" (allocation vs. field read). Any such recognizer
must keep the cache-range test as a **runtime** guard, not a compile-time
one, whenever the argument is not itself an `Op::Const` in range: the cache
range is a JLS-mandated *minimum*, not a fixed constant, and a JVM is free to
widen it, so hardcoding `-128..=127` into a fold would be sound only for the
provably-constant-and-in-the-documented-minimum-range case, which is also the
only case worth folding (a variable argument still needs the real call for
the identity to be observably correct either way).

**Why it is worth doing.** Nothing today — this is a "there is no bug here
because there is no feature here" finding, filed so a future round does not
have to re-discover that the boxing half is unimplemented before deciding
whether it is worth implementing.

**Cost.** Unscoped; this is a design question (does the box fast path pay for
itself against the `jit_invoke_dispatch` cost `ir_evidence.rs`'s module
header prices calls at) rather than an estimate.

## 3. `eliminate_redundant_loads` (load CSE) is default-OFF with no recorded
   soak, unlike every sibling flag in the same file

**Where:** `jit/src/ir_optimize.rs`, `load_cse_enabled` (line 1946) and
`load_cse_alias_enabled` (line 1938). Both read
`CRATONVM_JIT_IR_LOAD_CSE`/`CRATONVM_JIT_IR_LOAD_CSE_ALIAS` with the
`Ok("1") | Ok("true") | ...` **opt-in** shape — the same shape
`licm_hoist_counted_enabled`, `licm_before_unroll_enabled` and
`partial_unroll_enabled` use for a deliberately-off feature — but unlike
`licm_enabled`/`unroll_enabled` (which flipped to default-on after a recorded
soak, per their own doc comments citing specific benchmarks and a checksum
parity run), neither `load_cse_enabled` nor the doc comment above it
(`"default OFF while it is measured"`) names a soak that ran, a workload it
ran on, or a number.

**What is true today.** `eliminate_redundant_loads` is a real, tested pass
(`ir_load_cse_census`/`ir_load_cse_census_here` are wired into
`interp_census.rs`, so the counters exist and are read), gated behind a flag
that, per this round's FOCUS question #3 ("can this pass fire at all on a
production artifact"), is off by default and — as far as this lane's
read-only review can tell — has never been measured on. It is not a dead
pass (it has unit tests exercising both the plain and alias-aware arms), but
it is a shipped-and-inert one on any default configuration, which is exactly
the shape this round's brief calls out as worth naming even when it is not a
correctness bug.

**Proposal.** Run the same soak procedure `licm_enabled`'s and
`unroll_enabled`'s doc comments describe (bt10/14/16/18 plus a loop-heavy
differential gate-ON vs. gate-OFF comparison against HotSpot) with
`CRATONVM_JIT_IR_LOAD_CSE=1` and `CRATONVM_JIT_IR_LOAD_CSE_ALIAS=1`, using
`ir_load_cse_census()` the way `refusal_census()`/`range_census()` are
already used for the bounds-check-elimination flags, and flip the default
the same way if it is clean.

**Why it is worth doing.** It is the one pass in this file whose own doc
comment says it is provisional, and it is the shape the round's FOCUS
question is specifically asking every lane to name: a pass that plainly CAN
fire (it is not gated on an impossible condition, and its tests prove the
logic path is live) but by default does not, on any workload, because nobody
has closed out the measurement its own comment promises.

**Cost.** The soak itself, not a code change — `load_cse_enabled` already
reads the flag correctly and `ir_load_cse_census` already reports the right
numbers; this is a "spend the measurement" item, not an implementation one.

## 4. `ir_check_elim`'s `REFUSAL_IV_SHAPE`/`REFUSAL_NOT_NON_NEGATIVE` split is
   already the work list; the two arms most worth relaxing are named in its
   own comments

**Where:** `jit/src/ir_check_elim.rs`, `proven_non_negative` (line 684) and
`is_unit_stride_induction` (line 756).

**What is true today.** The range-based bounds-check elimination already
separates its refusals into "an induction variable the unit-stride/back-edge
rule rejected" (`REFUSAL_IV_SHAPE`) from "a non-phi index this tier has no
value-range shape for" (`REFUSAL_NOT_NON_NEGATIVE`), specifically so that
`refusal_census()` can say which conjunct is worth extending next rather than
guessing. This lane's review confirms the module's own soundness argument for
the unit-stride restriction (`s == 1` is required because `len - 1 + s` can
overflow `i32` for any `s > 1` against a near-`i32::MAX` array) is correct and
should not be loosened by widening the stride bound.

**Proposal.** The safe way to recover `REFUSAL_IV_SHAPE` cases without
touching the overflow argument is to let `is_unit_stride_induction` accept a
stride that is itself proved to be **small and positive by the SSA range
lattice** (`node_ranges` is already threaded into `analyze` and is unused by
this function today) rather than only the literal constant `1` — e.g. a
stride proved `1 <= s <= k` for a `k` the accompanying upper-bound proof can
still show does not overflow against the SPECIFIC array's length when that
length is itself bounded (not `i32::MAX` in the worst case, but a
`newarray`'d local of known small bound). This is a narrower, still-sound
extension of the existing proof rather than a relaxation of it, and it is
exactly the kind of extension the refusal census exists to justify with a
real distribution rather than a guess.

**Why it is worth doing.** `bounds_elided=8` against `bounds_emitted=174` on
H2 (the number `ir_check_elim.rs`'s own module comment cites for the
dominance-only pass before the range pass existed) is the reason the range
pass was built at all; the same census infrastructure, now split by refusal
reason, is what should decide whether this specific extension is worth its
complexity before anyone writes it.

**Cost.** Requires a real corpus's `refusal_census()` output to be worth
scoping further — this proposal is "look at the census before guessing",
which this lane could not do (no build, no run).
