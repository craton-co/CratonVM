# Activate the IR Optimizer (GVN / const-fold / DSE / LICM + scalar replacement)

Status: design / partially-built-mostly-dormant. L. The Sea-of-Nodes IR and its
optimization passes exist and are unit-tested, but the live JIT path only
exercises them on a **narrow gated subset** of methods. This plan turns the
passes on broadly behind safety gates, and opens the escape-analysis →
scalar-replacement second front.

## Goal

Run GVN, constant folding, algebraic simplification, dead-code/dead-store
elimination, and loop-invariant code motion (LICM) on the IR for the **general**
case (not just call-free branch-free leaves), and make escape analysis →
scalar replacement fire on real allocation-heavy code — each behind a soak gate
with a correctness fallback to the single-pass backend.

## Current state (cited)

The machinery is real; the gates keep it mostly off.

- **The optimizer runs the right passes — but only on a tiny method shape.**
  `jit/src/ir_optimize.rs:18` `optimize()` runs a fixed-point loop of
  `fold_constants` (`:269`), `algebraic_simplify` (`:456`), `gvn` (`:579`),
  `eliminate_dead_nodes` (`:625`), plus optional affine reassociation
  (`reassociate_affine`, `:191`) gated on `CRATONVM_JIT_REASSOC`
  (`reassoc_enabled`, `:43`).
- **But the IR path is gated to "call-free, branch-free (or REASSOC-opt-in)".**
  `jit/src/lib.rs:4142`–`4193`: the IR backend is taken only if the method
  builds an IR graph *and* is not a "pure call-free branchy" method — that exact
  shape is declined (`:4163`–`4167`) because **the IR φ/branch lowering
  miscompiles** it (`:4143` comment: a tiny `(m & K) != 0` predicate SIGSEGVs a
  write through a near-null base). Methods with `invoke*` lower correctly, and
  branch-free leaves are fine; the gate exists purely to dodge the φ/branch bug.
  So today the optimizer mostly sees branch-free leaf methods or
  REASSOC-opt-in branchy integer kernels.
- **Escape analysis IS wired into that path — but its win is bounded by the
  same gate.** `lib.rs:4174`–`4182`: after `optimize()`, the IR is converted to
  an `escape_analysis::Graph` (`escape_analysis_from_ir`, `lib.rs:3548`),
  `escape_analysis::analyze_escapes` runs (`:4176`), and
  `apply_ea_to_ir` applies scalar replacement / lock elision when the result is
  non-empty. `jit/src/escape_analysis.rs` is 1936 lines of real analysis. But
  because it only runs on the narrow IR-eligible methods, it rarely fires on the
  allocation-heavy code that would benefit.
- **LICM exists in the lowerer.** `jit/src/ir_lower.rs:657` documents
  loop-invariant motion for `Op::LoadField`/`Op::LoadStatic`; `optimize()` is
  invoked from `ir_lower.rs:743`, `ir_schedule.rs:439`/`:530`.
- **SCEV and loop analysis exist, lightly wired.** `jit/src/scev.rs`
  (`analyze_induction_variables`, `:201`; `trip_count`, `:120`) and
  `jit/src/loop_analysis.rs` (`detect_loops`, `:90`; `find_invariant_loads`,
  `:147`) provide induction-variable / trip-count / invariant-load info. `scev`
  is currently used mostly for its `bytecode_len` walker (`lib.rs:3959`,
  `:4790`); the IV/trip-count analysis is not driving an aggressive unroll/LICM
  decision on the general path.
- **DSE is the notable gap.** There is `eliminate_dead_nodes` (pure-node DCE)
  but no *dead-store* elimination (eliminating a `StoreField`/`StoreStatic`
  whose value is overwritten before any read) on the general path.

Net: passes are correct and tested; the **φ/branch lowering bug** (`lib.rs:4143`)
is the dam holding back broad activation, and DSE / aggressive LICM-via-SCEV are
not yet on.

## Design

### Front 1 — broaden the IR path (fix the dam, then open the gate)

1. **Root-fix the IR φ/branch lowering bug** (`lib.rs:4143` SIGSEGV on a
   call-free branchy predicate). The write-through-near-null symptom points at a
   φ resolution / branch-target patch that produces a bad base address. This is
   the prerequisite — until it's fixed, every branchy method must keep falling
   back to the single-pass backend. Build a focused repro (the doc names
   `Modifier.isStatic`-style `(m & K) != 0` and keycloak JsonParserTest) and a
   φ-lowering self-check (compare IR-lowered vs single-pass results on a corpus).
2. **Relax the gate** (`lib.rs:4163`) incrementally: branch-free leaves →
   branchy call-free → methods with calls → loops. Each relaxation behind a
   `CRATONVM_JIT_IR_*` soak flag with a differential test against the single-pass
   backend.
3. **Keep the single-pass backend as the correctness fallback**: if IR lowering
   `None`s or a self-check fails, fall through to `x64.rs` (already the structure
   at `lib.rs:4193`).

### Front 2 — add DSE and make LICM/unroll SCEV-driven

1. **Dead-store elimination**: add an `ir_optimize` pass that removes a
   `StoreField`/`StoreStatic`/array store whose location is provably overwritten
   (or never read) before any aliasing load — with a conservative alias model
   (give up on any unknown base). Compose it into the `optimize()` fixed-point
   loop (`ir_optimize.rs:29`).
2. **SCEV-driven LICM + unrolling**: use `scev::analyze_induction_variables` /
   `trip_count` (`scev.rs:201`/`:120`) and `loop_analysis::find_invariant_loads`
   (`loop_analysis.rs:147`) to hoist invariant loads out of loops and to bound
   unrolling. The reassociation pass (`ir_optimize.rs:191`) already targets the
   unrolled-affine shape; pairing it with a real trip-count gate makes unroll
   decisions principled instead of `CRATONVM_JIT_REASSOC`-blanket.

### Front 3 — escape analysis → scalar replacement, broadly

1. Once Front 1 opens the gate, escape analysis (`escape_analysis.rs`,
   `lib.rs:4174`) runs on allocation-heavy code. The high-value case is a
   non-escaping object whose fields become SSA values (no heap alloc, no
   `<init>` call) — the same optimization whose *absence* and whose
   *over-application* are both documented hazards:
   - `MEMORY.md` "kafka bug-25": the single-pass escape pass
     (`x64.rs analyze_escapes`) **wrongly** scalar-replaced an object that
     escaped as a call argument. The IR escape analysis must not repeat this —
     any value flowing into a call arg, field store, array store, return, or
     throw **escapes**.
2. **Scalar replacement that survives a guard** is the *deep* win but requires
   `real-frame-deopt.md`: an object non-escaping on the fast path can be
   scalar-replaced even if a rare guard failure needs it re-materialized
   (`FrameValue::VirtualObject` / `materialize_virtual_objects`). Until deopt
   lands, restrict scalar replacement to objects non-escaping on **all** paths.

## Increment 1 (DSE + widened escape analysis) landed

Status: **landed** on `rm/activate-ir-optimizer`. This is the first landable
slice of Fronts 2 and 3, taken now that the φ/branch SIGSEGV dam is fixed on
`dev` (the branchy IR path is live, so these passes run on real branchy
methods).

**What landed**

1. **DSE pass (`jit/src/ir_optimize.rs`, `eliminate_dead_stores`)** — a new
   dead-*store* pass, distinct from the existing pure-node DCE
   (`eliminate_dead_nodes`). It removes a `Store` whose written location is
   overwritten by a later store with no intervening reader. It is wired into
   the `optimize()` fixed-point loop *before* `eliminate_dead_nodes` so the
   freed value chains get DCE'd in the same iteration. Soundness guards:
   - Only stores to a **provably-local allocation** (`New`/`NewArray` base)
     are ever removed; any Param / loaded-ref / call-result / unknown base is
     left untouched.
   - The "overwritten before any read" check is a straight-line scan in
     node-id order. Every non-pure, non-store node — including all control /
     merge / phi / projection nodes and any `Load`/`Call`/`Return`/`Guard`/
     monitor — is a **memory barrier** that flushes the pending set, so two
     stores only ever match inside one straight-line region (where node-id
     order is a valid before/after relation). This dodges the "node-id order
     ≠ global program order" hazard without a real alias oracle.
   - The location key is structural `(base_node, index_node, MemKind)`; two
     distinct `New` nodes never alias, so an interleaved store to a different
     local allocation does not flush.
   - The `Store` operand reader (`store_operands`) tolerates both the compact
     `[base, value]` layout (EA bridge / hand-built graphs) and the full
     `[ctrl, mem, base, index, value]` layout; an unrecognised layout is
     treated as a barrier (never removed).

2. **Widened escape → scalar replacement (`jit/src/escape_analysis.rs`,
   `find_scalar_replacements`)** — the candidate walk was rewritten from a
   single-level use scan (which bailed via the catch-all on *any* non-
   load/store use) to a transparent-alias worklist:
   - **`Op::Dead` uses are skipped**, not rejected (a stale dead use-edge
     observes nothing and must not block SR).
   - **A `NoEscape` `Op::Phi` that provably aliases *only* this allocation is
     treated as a transparent copy**: its onward field loads/stores are
     folded just like direct ones. This fires SR on the
     `o = (cond ? o : o)`-through-a-merge shape the narrow scan rejected.
   - Soundness: the phi is accepted only when (a) its resolved points-to set
     is the singleton `{alloc}` **and** (b) *every* reference-producing input
     resolves to exactly `{alloc}`. Guard (b) closes the kafka bug-25-class
     hole where a phi merging the allocation with an unknown reference
     (`Param`/`Call`/`Load` — which contribute no points-to entry) would
     spuriously look like a singleton; folding a load through such a phi
     would miscompile the path that takes the foreign reference.

**Tests added** (run from the worktree):
- `cargo test -p jit dse` — DSE: removes an overwritten store, keeps a store
  observed by an intervening load, keeps a store to a non-local (Param) base,
  and does not match distinct fields.
- `cargo test -p jit scalar_replacement_through_transparent_phi` and the
  `phi_merging_*` / `skips_dead_use` cases — escape widening fires on the new
  shape and the two soundness guards (ambiguous phi, alloc+Param phi) bail.

**Not yet done** (still Fronts 2/3 follow-ups): SCEV-driven LICM/unroll, and
guard-surviving scalar replacement (gated on `real-frame-deopt.md`).

## Implementation steps (ordered)

1. **φ/branch lowering repro + fix** (Front 1.1) — unblocks everything.
2. **Differential self-check harness**: IR-lowered vs single-pass result
   equality on a method corpus; required before any gate relaxation.
3. **Relax the IR gate** branch-free → branchy → calls → loops, each behind a
   soak flag (Front 1.2).
4. **Add DSE** to `ir_optimize::optimize` (Front 2.1).
5. **SCEV-gated LICM + unroll** (Front 2.2).
6. **Broaden escape analysis** once the gate is open; enforce the
   "call-arg/store/return/throw ⇒ escapes" invariant (Front 3.1).
7. **Guard-surviving scalar replacement** after `real-frame-deopt.md` (Front 3.2).
8. **Flip defaults** pass-by-pass as each soaks clean on the app gauntlet +
   bt10/14/16/18 checksums.

## Risks

- **The φ/branch bug is a real miscompile**, not a perf knob — broadening the IR
  path before it's fixed reintroduces SIGSEGVs. The self-check harness is the
  gate.
- **Escape-analysis soundness** (kafka bug-25 class): a single missed escape
  edge → a scalar-replaced object that should have been heap-allocated →
  null/garbage at a real use. Conservative-on-unknown is mandatory.
- **DSE aliasing**: removing a store that *was* read through an aliased base is
  a silent data-loss bug. Give up on any base the analysis can't prove
  non-aliasing.
- **LICM hoisting a load past a store** to the same location, or past an
  exception edge that should observe the pre-loop value, is incorrect — respect
  memory effects and exception edges.
- **Checksum invariants**: every default flip must preserve bt18 = 68332206 and
  the kafka/keycloak/tomcat gauntlet baselines.

## Effort

L. Front 1.1 (φ/branch fix) is the keystone within this doc and is M–L on its
own. DSE and SCEV-LICM are each M. Broad escape analysis is M (analysis exists);
guard-surviving scalar replacement is gated on `real-frame-deopt.md`.
