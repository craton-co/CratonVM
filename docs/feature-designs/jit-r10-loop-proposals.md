# Loop-pass proposals — round 10, lane `loop`

**Status:** PROPOSALS ONLY. Written at the close of a hard review of
`jit/src/x64/licm.rs`, `licm_int.rs`, `loop_rewrite.rs`, `loop_unroll_admission.rs`,
`bce.rs`, `jit/src/loop_analysis.rs`, `scev.rs` and `range_analysis.rs`. Nothing
below was built or measured; each item states its own cost.

## 1. Retire the native byte-copy unroller in favour of the bytecode rewriter

**Where:** `jit/src/x64/loop_rewrite.rs` (`bytecode_loop_xform_rewrites_bytecode`,
`native_unroller_enabled`), `jit/src/x64/driver.rs`'s `plan_native_unroll` call
sites.

**What is true today.** Two unrollers exist and are mutually exclusive by
construction (`native_unroller_enabled() == !bytecode_loop_xform_rewrites_bytecode()`,
pinned by `the_two_unrollers_are_never_both_live` in
`loop_unroll_admission.rs`). The native one is live by default; the bytecode
rewriter is oracle-only (`plan_native_unroll` asks `plan_loop_unroll` for a
verdict and discards the rewritten bytes). `loop_rewrite.rs`'s own module doc
says the three whole-compile refusals that used to block the rewriter
(`deopt_real`, precise exception frames, `invokedynamic`) are gone — the bci
translation through `Compiler::orig_bci` answers all three — and the fourth
(inline sites) is a real limit on TODAY's provenance map, not a fundamental one.

**Proposal.** Default-on the bytecode rewriter for loops with no inline sites,
after: (a) soaking `loop_xform_deopt_frames_diverge` (counted, never refused,
per `rewritten_deopt_points_are_publishable`) on a real corpus to confirm it
stays at the noise floor; (b) extending inline-scope provenance
(`docs/jit/deopt-inline-scopes.md`) so `InlineSitesPresent` stops excluding
every inlined hot loop.

**Why it is worth doing.** The native unroller duplicates MACHINE CODE and
re-resolves helper `rel32`s and IC slots per copy — real work repeated `k`
times per compile. The rewriter duplicates BYTECODE once and lets the ordinary
emitter compile it, so IC slots, field resolutions and inline sites are
computed once per compile regardless of `k` (`replicate_pc_keyed`'s whole
point). It is also the only path that can PEEL a bypassable header
(`LoopXformKind::Peel`), which the native unroller cannot do at all — every
bypassable-header loop today keeps its unroll refused
(`plan_native_unroll` -> `bypassable_headers.contains(&header)` -> `None`)
rather than being peeled into an unrollable shape.

**Cost.** Inline-scope provenance is the real work — it means describing a
callee's bci space inside the caller's rewritten-bytecode coordinate system, a
change that touches inlining's snapshot machinery, not just this file. Rough
size: comparable to the OSR-entry / deopt-bci translation this lane's doc
comments describe as already landed, i.e. weeks, not days. Everything else
(the default flip, the soak) is small.

## 2. A guard SET for `plan_loop_version`, not one guard

**Where:** `jit/src/x64/licm.rs` (`encode_preheader_guard`, `LoopVersioning`),
`jit/src/x64/loop_rewrite.rs` (`trip_count_guard`, which already refuses when
`prove_trip_count_at_least` returns more than one guard: `[g] => Some(...), _ =>
None`).

**What is true today.** `TripCountProof::Guarded` can legitimately carry more
than one `PreheaderGuard` (a `StrideInRange` term plus a numeric one, for a
runtime-strided loop with an unproven entry value). `trip_count_guard` throws
the whole obligation away the moment there is more than one, falling back to
the unguarded transform. `encode_preheader_guard`'s doc is explicit that this
is a real restriction ("Nothing with two comparisons... would need two fallback
edges") and not an oversight.

**Proposal.** Extend `LoopVersioning`'s layout with `Vec<(PreheaderGuard, pc,
len)>` chained ANDs — each guard's failure edge targets the NEXT guard's
first byte, and only the last guard's failure edge targets `fallback_base` —
and let `plan_versioned` hand over every guard `prove_trip_count_at_least`
returns instead of discarding all but a singleton. `GuardShape::covers` in
`bce.rs` already answers per-guard coverage questions the same way a chained
encoder would need to.

**Why it is worth doing.** Every variable-stride loop (`for (i = k; i < n; i +=
step)`, the Sieve-of-Eratosthenes inner loop this codebase's own tests use as
the canonical example) needs both a `StrideInRange` guard and a numeric
trip-count guard to be versioned at all. Today it gets neither: one guard
lost, the other never asked for.

**Cost.** Small and mechanical in `licm.rs` (a `Vec` instead of one guard, one
extra loop over `encode_preheader_guard` outputs, chaining offsets). No change
to `PreheaderGuard`'s type — this is purely about how many of them
`plan_loop_version` accepts.

## 3. Sharper `Range::ushr` / `Range::or` / `Range::xor` for mixed-sign inputs

**Where:** `jit/src/range_analysis.rs` (`ushr`, `or`, `xor`, `and`'s
`(false, false)` arm).

**What is true today.** `or` and `xor` answer `Range::top` the moment either
operand's range includes a negative value; `ushr` answers a width-wide
"non-negative, below `2^(bits-s)`" bound whenever the input's lower endpoint is
negative, with no use of the upper endpoint at all. Each of these is SOUND
(documented and verified sound above) but throws away precision the interval
already has: a range entirely below zero (`hi < 0`) has an exactly computable
`ushr` image (unsigned representation is monotonic in the value for an
all-negative sub-range), and `or`/`xor` of two ranges that are each entirely
negative or entirely non-negative (not just "some value is") have tighter
closed forms than top.

**Proposal.** Split each transfer function's negative case into "entirely
negative" (`hi < 0`) vs. "straddles zero" (`lo < 0 <= hi`), and give the first
sub-case the monotonic closed form instead of falling through to the
imprecise combined case. This is the same shape `and`'s `(false, false)` arm
already uses (`if ah < 0 && bh < 0 { tight } else { top }`) — `or`/`xor`/`ushr`
would gain the analogous split.

**Why it is worth doing.** `hash & (table.length - 1)` is the one masking
idiom `and` already special-cases and the comment names as load-bearing for
hashed-index bounds checks. `x | Integer.MIN_VALUE` (setting a bit) and
`x >>> 1` on a provably-negative `x` (bit-manipulation code, `Long.hashCode`,
checksum accumulators) are the same shape one level over — currently every one
of them returns top the instant the analysis has proven the input negative
at all, undoing the very fact that made the proof interesting.

**Cost.** Small: pure-function changes to three transfer functions, each
provable by the same exact-arithmetic-then-narrow discipline the module
already states as its rule, plus a handful of `#[cfg(test)]` cases per
function (a fully-negative pair, a straddling pair, a fully-non-negative
pair, and the constant-count zero-shift identity). No callers change.

## 4. A shared "does this bytecode walk model `wide`?" lint, not eight retellings

**Where:** every file in this lane's ownership list.

**What is true today.** This codebase has fixed the "a `wide`-prefixed store
is invisible to a linear bytecode walk" bug class at least three times on
record (`loop_analysis::modified_locals_strict`'s doc history, `bce.rs`'s
`find_induction_variable` `0xc4` arm, and the `licm_int.rs` /
`licm.rs::find_array_len_hoists` comments about why they refuse a body
containing one at all rather than re-deriving the fix). Each fix is a
hand-written match arm plus a hand-written regression test; nothing enforces
that walk N+1 gets the same treatment.

**Proposal.** A `#[cfg(test)]`-only differential harness — call it
`assert_wide_prefix_parity` — that, given a bytecode buffer and a "does this
opcode write local L" predicate over the SHORT form, checks the predicate's
answer against the `wide`-prefixed encoding of the same instruction at the
same PC, for every opcode that has a `wide` form (`iload`/`lload`/`fload`/
`dload`/`aload`, their stores, and `iinc`). Every one of this lane's linear
walks that touches locals (`find_induction_variable`, `find_iv_stride`,
`modified_locals_strict`, `find_iv_nonneg_start`,
`bound_arraylength_provenance_with`, the arith/array-length/FP hoist finders)
runs it once in its own test module.

**Why it is worth doing.** The bug class is not hypothetical — it has cost a
real fix three times in this codebase's own history, and by construction it
is invisible to a reviewer reading any ONE of the eight files, because each
walk is a self-contained match over opcodes and "opcode 0xc4 has no arm" reads
as a boring default case, not a hole. A shared harness turns "did we remember
`wide` this time" from a per-function manual audit into a one-line call.

**Cost.** Small (a test-only helper, driven by a table of `(short_opcode,
wide_real_opcode, operand_len)` this lane's files already enumerate
individually in comments) plus one call site per existing walk. No production
code changes.

## 5. Publish `loop_xform_deopt_frames_diverge` and `loop_xform_planner_refused` as a per-refusal-reason histogram

**Where:** `jit/src/x64/loop_rewrite.rs` (`metrics::record_loop_xform_event`),
`jit/src/x64/licm.rs` (`LoopXformRefusal`).

**What is true today.** `plan_bytecode_loop_xform` tallies `loop_xform_*`
counters by CATEGORY (`loop_xform_planner_refused` vs.
`loop_xform_no_candidate_loop`), but the actual `LoopXformRefusal` variant
(there are seventeen) is only ever logged behind
`CRATONVM_DBG_JIT_GEN`, never counted. So today's oracle-only mode (item 1's
prerequisite for even being SOAKED) cannot answer "which refusal is costing
the most loops" without re-running a debug-logged corpus and grepping.

**Proposal.** A `[LoopXformRefusal; N]`-shaped counter array (or a
`FxHashMap<&'static str, AtomicU64>` keyed on `Debug` output, matching the
existing `record_loop_xform_event` string-keyed style) bumped once per
`Err(refusal)` in `plan_bytecode_loop_xform`, independent of the debug-log
flag.

**Why it is worth doing.** It is the measurement item 1's own soak plan
needs, and it is what would have told this lane, without reading source,
whether `BodyTooLarge` or `ExternalEntry` or `TimeToSafepointBudget` is the
dominant refusal on a real corpus — which is exactly the question "should
this default flip" depends on.

**Cost.** Trivial — the refusal is already computed and matched on at the one
call site; this only adds a counter bump next to the existing debug print.
