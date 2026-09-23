# Wiring the `VecPlan` vectorizer in — what it would take, and what it buys

Written round 10 wave 5, lane `vecplan`. Everything below is from reading the
tree on 2026-09-21; this lane was not permitted to build, so no number here was
measured by it and none is presented as if it were.

## The state of play, precisely

Two files hold a complete, carefully-argued auto-vectorizer:

* `jit/src/x64/simd_analysis.rs`, module `vector_gate` — decides whether a
  counted loop may be widened. Emits nothing. Produces a `VecPlan`: lane count,
  element type, alignment verdict, tail strategy, overflow model, dependence
  list and a set of `PreheaderGuard`s the consumer must discharge in full.
* `jit/src/x64/vec_emit.rs` — turns an admitted `VecPlan` plus a
  `VecLoopShape` (which machine registers hold what) into bytes, or refuses.
  One loop shape, guards first, scalar remainder by fall-through, VEX
  encodings only.

Both are extensively tested at the unit level. Neither has a production caller.
Searched across the repo (`rg`, read not executed):

| symbol | callers outside its own file |
|---|---|
| `vector_gate::admit_vectorization` | none (one prose mention in `x64/driver.rs`) |
| `vec_emit::emit_vector_loop` | none (three prose mentions in `regalloc.rs`) |
| `VectorIsa::detect` | none |
| `HostVectorSupport::detect` | none, and until round 10 not even a test |
| `VecEmitPolicy::from_flags` | none (`types/src/flag_groups.rs` records the fact) |

The SIMD that ships today is a different, hand-written pipeline —
`jit/src/x64/simd.rs`'s `emit_simd_int_array_sum` and
`emit_simd_int_array_element_wise`, admitted by bytecode pattern matching in
`simd_analysis.rs`'s *detector* half and gated by `single_pass_only.rs` calling
`has_avx2()` directly. It does not go through `VectorIsa`, `VecPlan` or
`vec_emit` at all. So the question "what would it take to wire the `VecPlan`
pipeline in" is not "add a call" — the two halves of the JIT that would have to
supply its inputs cannot currently both see the same loop.

## Proposal 1 — bridge the bytecode/SSA split that blocks `VecCandidate`

**This is the blocking item. Everything else is downstream of it.**

`admit_vectorization(graph: &Graph, cand: &VecCandidate)` wants, in one place:

* `graph: &crate::ir::Graph` and `VecBodyOp { node: NodeId, effect: MemEffect,
  … }` — **SSA**, because the dependence test asks `Graph::may_alias` over
  `AliasClass` and refuses to decide disjointness itself;
* `cand.counted: &crate::scev::CountedLoop` and `IndexExpr { iv_local: usize,
  scale, offset }` — **bytecode**, because `scev` is a bytecode analysis and
  `iv_local` is a JVM local slot number.

Nobody holds both. `ir_optimize.rs` says so in its own words, in the LICM
section header:

> `scev::CountedLoop` / `loop_analysis::analyze_counted_loop_at` are *bytecode*
> analyses … They are not threaded through `optimize(&mut Graph)` (which only
> sees the post-build SSA graph, no bytecode), and their PC keys do not map onto
> SSA `NodeId`s, so they cannot *drive* node-level hoisting directly.

and it acts on that: `ir_optimize.rs` carries its **own private**
`struct CountedLoop { iv_phi: NodeId, iv_init, iv_stride, trip, if_node,
exit_ctrl }`, keyed on `NodeId`, built by its own private
`analyze_counted_loop(graph, region, back)` — not the type, and not the
function, the gate takes.

The gate's `scev::CountedLoop` comes from
`loop_analysis::analyze_counted_loop_at*`, whose first parameter is
`code: &[u8]`. Its consumers in the backend are
`x64/loop_rewrite.rs` (which calls `analyze_counted_loop_at_with_handlers`
directly) and `x64/bce.rs` (which holds a `counted: CountedLoop` and drives the
`scev` proofs from it). Both are in the **single-pass bytecode** backend, which
has no `Graph`. So the two halves of `VecCandidate` are on opposite sides of a
split the tree has already decided not to bridge.

Three ways out, in increasing order of how much they cost and how much they
are worth:

**1a. Make `VecCandidate` SSA-only (recommended).** Replace `counted:
&scev::CountedLoop` with a small SSA-keyed trait or struct supplying the five
facts the gate actually asks for — `index_span`, `prove_index_in_bounds_of`,
`trip_count`, `prove_trip_count_at_least`, and `iv.stride.as_const()` — and
replace `IndexExpr::iv_local` with the induction `Phi`'s `NodeId`. The gate's
text never needs a bytecode local; it needs "is this subscript the loop's IV,
times one, plus a constant". `ir_optimize`'s existing loop recognition already
finds the induction phi and its constant stride. This keeps the gate's
"nothing here re-derives a fact another pass proves" discipline, moves the
proofs it delegates to behind one seam, and is the only option that leaves the
pipeline callable from where the aliasing information lives.

*Cost:* touches `simd_analysis.rs` (the `VecCandidate`/`IndexExpr` interface
and `classify_body_op`), and needs the four `scev` proofs re-expressed against
SSA ranges. The bounds proofs are the expensive part; an SSA `RangeEnv` is what
`range_analysis.rs` already is, so check there first.

**1b. Thread bytecode facts into the IR tier.** Keep `scev::CountedLoop`, and
teach the bytecode→IR builder to record, per loop header `Region`/`Merge`, the
bytecode PC range and the local slot its induction phi came from. Cheaper to
start, but it recreates the PC↔NodeId mapping `ir_optimize` explicitly declined
to build, and every later transform invalidates it silently.

**1c. Run the gate in the single-pass backend instead.** `bce.rs` already has
`scev::CountedLoop`, `RangeEnv`, and a `PreheaderGuard` emitter. But it has no
`Graph`, so `Graph::may_alias` is unavailable and the dependence test — the
whole reason this gate exists rather than the pattern matcher in `simd.rs` —
would have to be replaced by something weaker. That is the existing
`simd.rs` pipeline with extra ceremony. Not recommended.

**What it buys.** The `simd.rs` detectors recognise three fixed source shapes
(int-array sum, int-array element-wise, matrix dot). `admit_vectorization`
recognises *any* unit-stride counted loop over one element type whose
dependences allow widening — element-wise float/double arithmetic, `|`/`^`/`&`
reductions, multi-array bodies, and loops with an offset subscript
(`c[i] = a[i] + b[i+1]`), none of which the detectors match. It also admits
loops the detectors reject for reasons that are no longer true of the emitter
(a runtime `a.length` bound, a non-zero start index).

## Proposal 2 — give the caller the width table, and close the sub-register gap

Filed separately as
`docs/internal/retired/r10-vecplan-dependence-capped-widths-are-admitted-but-never-emittable-20260921-RETIRED-20260922.md`.

Short form: a 4-byte element with a backward dependence distance of 2 or 3
yields `lanes = 2`, `width_bytes = 8`, which `emit_vector_loop` always refuses
(`UnsupportedWidth`), because its width table is exactly `{16, 32}`. The gate
is deliberately ISA-generic and should not learn the x86 table; the emitter
should not be handed an 8-byte encoder written blind.

The clean resolution belongs to the call site added by proposal 1: it owns both
ends, so it can ask the emitter what widths it supports (a
`vec_emit::supported_widths(isa) -> &[usize]`, or simply a
`min_width_bytes(isa)`) and floor the lane cap with it before calling
`admit_vectorization` — or, having got a plan it cannot emit, re-run the gate at
the next width down. Either way the knowledge stays in the emitter and the
*policy* stays in the caller, which is where it already is for the pool, the
frame save set and the flag.

## Proposal 3 — `cpu_features::has_avx()`, and a 128-bit VEX tier

Round 10 narrowed `VectorIsa::detect()` to AVX2-or-`None` on x86-64 because
every encoding in `vec_emit` is VEX and `HostVectorSupport` carries one bit.
That is correct today and it is also the *smaller* of the two true statements.

The larger one: the 16-byte paths in `vec_emit` are already written, already
tested byte-for-byte, and need only **AVX**. `VPADDD xmm`, `VMOVDQU xmm`,
`VPSHUFD xmm`, `VMOVD` and `VZEROUPPER` are all AVX; only `VEX.256` integer
forms (and therefore `VEXTRACTI128`, which the 128-bit reduction path already
skips) need AVX2. So a host with AVX and no AVX2 can run this emitter at
`width_bytes == 16` with **no new encoder at all**.

What it takes:

1. `jit/src/x64/cpu_features.rs`: expose `has_avx()`. The bit is already read —
   `detect_avx2` checks `CPUID.1:ECX[27]` (OSXSAVE) and `[28]` (AVX) and
   `XGETBV` before it looks at leaf 7 — it is simply not published. Factor that
   prefix into `has_avx()` and have `detect_avx2` call it, so the two cannot
   drift.
2. `vec_emit::HostVectorSupport`: add `avx: bool`, keep `avx2`, and make the
   entry gate refuse on `!avx` rather than `!avx2`; refuse a **32-byte** plan on
   `!avx2` with the existing `HostLacksAvx2`.
3. `VectorIsa::detect()`: three-way again, but this time honestly — `avx2()`,
   then a new `avx128()` (`width_bytes: 16`, `int32_mul_minmax: true`,
   `UnalignedOk`), then `None`. The `sse2()`/`sse41()` constructors stay what
   they became: explicitly-named modelling targets, not host answers.
4. The invariant tests written in round 10
   (`x86_host_detection_is_avx2_or_nothing`,
   `the_gate_only_offers_an_isa_this_emitter_can_encode`) are exactly the
   assertions that must be rewritten by this change, and their doc comments say
   what they rest on so the rewrite is mechanical rather than archaeological.

**What it buys.** Sandy Bridge and Ivy Bridge (AVX, no AVX2) are still common
in cloud fleets and in CI images, and — the case that is easy to forget — a
hypervisor that masks AVX2 while passing AVX through puts a modern part in this
tier too. `cpu_features.rs`'s own comment on `detect_avx2` describes exactly
that hypervisor shape. Today every one of those hosts gets no vectorization
from this pipeline at all. Half-width is not half-speed, but it is not nothing,
and it costs one CPUID bit and one field.

**Do it after proposal 1, not before.** Until something calls the emitter, this
widens the set of plans nobody builds.

## Proposal 4 — land the observability with the call site, not after it

Round 10 found the same defect in four separate lanes: a counter or a public
API with no production caller, reading zero forever, indistinguishable from
"the thing it counts never happened". This module is the extreme case of the
shape — five symbols, zero callers, including `HostVectorSupport::detect`,
which until this round was not even called by a test, so nothing anywhere
checked that it agreed with `cpu_features`.

So the proposal is a rule as much as a feature: **the call site added by
proposal 1 lands with its reader, in the same change.** Concretely:

* Count the refusal taxonomy, not just the successes. `VecRefusal` has 20-odd
  variants and `VecEmitRefusal` about 25, each of which is a specific,
  actionable "this loop was nearly vectorizable except…". A histogram of them
  over a benchmark run is the single most useful artifact this pipeline can
  produce before it produces a single vector instruction — it says which
  refusal to attack next, which is otherwise guesswork.
* Put it where a reader already exists: `jit/src/metrics.rs` has
  `CompilationReport`, `Phase` and a JSON serialiser, and `-XX`-style flag
  plumbing lives in `types/src/flag_groups.rs` next to the existing
  `CRATONVM_JIT_VECTORIZE` entry. A new counter bank with its own private
  printer is how the last four of these got orphaned.
* Add the assertion that makes an orphan visible: a test that the counter is
  non-zero after compiling a method the pipeline is known to refuse. A counter
  whose only test is "it increments when I increment it" is the bug, not the
  fix.

## Proposal 5 — differential-check the emitted bytes against a disassembler

Every encoding in `vec_emit` is asserted as a hand-derived byte literal, and the
module's own test comments record that the derivation has been wrong at least
once (`an_int_sum_reduction_emits_the_zeroing_init_and_the_fold_tree`: "The
re-derivation got this one wrong first; the emitter was right"). Round 10 added
byte coverage for the last unasserted encoder path (`alu_ri`'s `81 /ext id`
form — see `a_guard_limit_past_the_imm8_range_uses_the_imm32_form`), so the
table is now complete, but "complete" here means "every path has a literal a
human wrote", which is the same authority twice.

A decoder exists in the tree, but not where it can be used from here, and the
detail matters:

* `iced-x86` **1.21**, `default-features = false`, features `["std",
  "decoder", "nasm"]`, is a dependency of the **`vm`** crate only
  (`vm/Cargo.toml`). Its one consumer is `vm/src/jit/disasm.rs`, the
  `CRATONVM_DBG_JIT_DISASM` dump, whose module doc states the constraint
  plainly: "iced-x86 is decode-only here (no encoder)". Decode-only is exactly
  what this proposal needs — the question is what the bytes *say*, not how to
  produce them a second way.
* The **`jit`** crate does not depend on it, in `[dependencies]` or
  `[dev-dependencies]` (`jit/Cargo.toml`). And the code under test is
  `pub(crate)` to `jit`, so a test living in the `vm` crate cannot reach
  `vec_emit::emit_vector_loop` at all.

So the concrete shape is: add `iced-x86` (same version and feature set, so the
workspace resolves one copy) to `jit`'s `[dev-dependencies]`, and put the check
in `vec_emit`'s own `#[cfg(test)]` module, where it can call the emitter
directly and decode the `VecLoopCode::code` it gets back. Assert mnemonics and
operands — `vpaddd ymm8, ymm8, ymm9`, `vmovdqu ymm8, [rcx+rbx*4+0x10]` — rather
than bytes. That catches the one class of error a hand-derived literal cannot:
the case where the test and the emitter make the *same* mistake.

`difftest/src/jitfuzz.rs` is the other half of a fuller version of this —
generate random admissible `VecLoopShape`s, emit, decode, and check the decode
against the shape — but that is a larger build and the dev-dependency version
is worth having first. Either way this is the one item on this page that can be
done today, independently of proposal 1, because it needs no call site.

## Proposal 6 — the three things the emitter refuses on purpose, ranked

Not blockers; recorded so the next reader does not have to re-derive why each
is hard.

1. **Sub-word elements (`byte`, `char`, `short`).** Refused because JVM
   arithmetic on them is performed in `int` and narrowed at the store, so a
   lane-wise `PADDB`/`PADDW` wraps at the wrong width. As the emitter's module
   doc already notes, the transform *is* exact for `+ - * & | ^` whose result
   goes straight to a same-width store. The missing piece is one bit on
   `VecStep`: "this value is only ever narrowed". Cheapest of the three, and
   `byte[]`/`char[]` loops are common (string and I/O code).
2. **`&` and `*` reductions, and `long` reductions.** Refused because the
   accumulator cannot be zero-initialised: `&`'s identity is all-ones and `*`'s
   is one, both of which need a materialised constant, and `VPMULLQ` is
   AVX-512. Materialising a constant needs a constant pool slot or a
   `VPCMPEQD acc, acc, acc` (all-ones in one instruction, no memory) — the
   latter makes `&` nearly free and is worth doing when a real `&`-reduction
   shows up in a profile.
3. **Masked tails.** Refused because they need AVX-512 predicate registers,
   which `cpu_features` does not detect and no `VectorIsa` models
   (`masked_tail` is `false` on every one). The scalar-remainder fall-through
   is correct and costs at most `lanes - 1` scalar iterations, so this is a
   pure-performance item and the last one to want.
