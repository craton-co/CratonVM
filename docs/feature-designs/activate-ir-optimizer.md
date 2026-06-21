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
- **DSE was the notable gap — now landed** (increments 1/2/4). Alongside
  `eliminate_dead_nodes` (pure-node DCE) there is now `eliminate_dead_stores`
  (overwrite + write-only dead-*store* elimination, with the load-alias
  refinement of increment 4). The original-state gap this bullet described is
  closed; see the increment notes below.

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

## Increment 2 (LICM + widened DSE)

Status: **landed** on `rm/activate-ir-optimizer`. The second landable slice of
Front 2 — the LICM half of Front 2.2 and the write-only widening of DSE
(Front 2.1).

**What landed**

1. **SCEV-driven LICM (`jit/src/ir_optimize.rs`, `licm` + helpers)** — a new
   loop-invariant code-motion pass, gated **default-OFF** behind
   `CRATONVM_JIT_LICM` (`licm_enabled`) while it soaks, and run once *after*
   the fixed-point cleanup in `optimize()` (followed by a final GVN + DCE when
   it changed anything). It hoists provably loop-invariant **loads** out of a
   natural loop into the loop pre-header:
   - **Loop model is structural on the SSA graph**: a loop header is an
     `Op::Region` (`[ctrl_entry, ctrl_backedge, …]`); the induction / loop-
     carried value is the `Op::Phi` anchored at that Region. `loop_body`
     computes the natural-loop body precisely as *forward-reachable-from-header
     ∩ can-reach-a-back-edge*, which excludes the loop-exit projection and all
     post-loop code (a reducible-loop body).
   - **Why structural, not bytecode-SCEV-driven**: `scev::analyze_induction_
     variables` / `trip_count` and `loop_analysis::detect_loops` /
     `find_invariant_loads` are *bytecode* analyses keyed by bytecode PC; they
     are not threaded through `optimize(&mut Graph)` (which has no bytecode in
     scope) and their PC keys do not map onto SSA `NodeId`s, so they cannot
     drive node-level hoisting directly. The pass therefore reasons on the
     graph and exposes `licm_scev_corroborates(code, code_len)` — a bridge that
     calls `loop_analysis::detect_loops` + `scev::analyze_induction_variables` +
     `loop_analysis::find_invariant_loads` to corroborate a counted/invariant-
     load loop from bytecode. It is the ready hook for a future bytecode-
     threaded caller and keeps both modules exercised from this module.
   - **Soundness guards**: hoist only a `Load` whose base AND address operands
     are loop-invariant (`is_loop_invariant`, conservative — *any* `Phi`, any
     in-body node, any unresolved id ⇒ variant) AND only when the loop body has
     **no memory barrier** (`loop_has_memory_barrier`: any in-body store /
     call / allocation / guard ⇒ bail). No alias oracle: one barrier
     disqualifies all loads. Pure invariant nodes already float in Sea-of-
     Nodes, so the pass does not move them (it relies on scheduling + the
     trailing GVN to dedup loop-entry vs loop-body copies).
   - **Pre-header**: the region's entry-edge control (`inputs[0]`); a hoisted
     full-form load's control input is repointed there. Compact-form loads
     (no control slot) are left to scheduling, with `changed` set so the
     trailing cleanup runs.

2. **Widened DSE — write-only, never-read (`eliminate_write_only_stores`)** —
   the existing straight-line overwrite phase only fires inside one barrier-
   free region. The new second phase is a path-insensitive whole-graph pass:
   a `Store` into a local `New`/`NewArray` allocation is removed iff (a) the
   allocation is **never read** (no `Load` addresses it), (b) it **never
   escapes** (it flows nowhere except as the *base* of a Store — any use as a
   Call arg, `Return` value, Store *value*, `ArrayLength`, `Phi`/`Proj`, etc.
   disqualifies it, closing the kafka bug-25-class escape hole), and (c) the
   store node itself has **no consumers** (use-count 0), so removing it never
   severs a memory/effect edge a consumer relies on.
   - **Array-element stores deliberately left out of the *overwrite* key**:
     the production bytecode→IR builder (`ir.rs`) never emits
     `Op::Store`/`Op::NewArray`/`Op::Load` (it bails on those opcodes), so the
     only stores that exist are EA-bridge / hand-built **compact-form**
     `[base, value]` nodes — there is no distinct, resolvable array-element
     index operand to extend the overwrite location key with. Until a real
     array `Store` operand layout lands, array-element *overwrite* matching is
     omitted; the write-only phase is layout-agnostic (keys on the base only)
     and already removes write-only dead array stores.

**Tests added** (run from the worktree):
- `cargo test -p cratonvm-jit licm` — LICM: hoists an invariant load out of a
  barrier-free loop; does NOT hoist a load whose base is the induction phi
  (loop-variant); bails when the body has a store barrier; and the SCEV/loop-
  analysis bridge corroborates a counted loop.
- `cargo test -p cratonvm-jit dse` — adds `test_dse_removes_write_only_never_
  read_store` (single write-only store removed) and
  `test_dse_keeps_write_only_store_when_alloc_escapes` (escaping alloc's store
  kept); existing overwrite-phase tests retained (one adjusted to read its
  allocation so it isolates the overwrite property from the new write-only
  phase).

**Not yet done** (still Fronts 2/3 follow-ups): threading bytecode into
`optimize()` so LICM can consult `licm_scev_corroborates` as a gate, a real
alias oracle so LICM can hoist loads past non-aliasing in-loop stores, the array
`Store` operand layout for array-element overwrite DSE, and guard-surviving
scalar replacement (gated on `real-frame-deopt.md`). LICM/unroll default flip is
gated on the soak.

## Increment 3 (full unrolling of small constant-trip counted loops) landed

Status: **landed** on `rm/iropt-unroll`, gated default-OFF behind
`CRATONVM_JIT_UNROLL`. Implemented graph-level loop unrolling from scratch (the
IR had no node-cloning machinery) and validated it live against HotSpot.

**What landed** (`jit/src/ir_optimize.rs`, wired into `optimize()` after the
fixed-point cleanup, before LICM):

- **`unroll`** fully unrolls a single-back-edge, single-block, side-effect-free
  counted loop whose trip count is a compile-time constant `<= UNROLL_MAX_TRIP`
  (8). It clones the per-iteration computation once per iteration with the
  induction variable substituted by its concrete constant and each carried phi
  by its running value, redirects post-loop uses of each carried phi to its
  final value, and straight-lines the control. The trailing fixed-point cleanup
  then folds the concrete induction values, collapsing the loop body to
  straight-line (often constant) code.
- **Real loop-shape discovery**: the bytecode→IR builder encodes a javac loop
  header as an `Op::Merge` with a back-edge (NOT `Op::Region`, which `loop_body`/
  LICM assume), and wraps the exit `If`'s projections in single-input `Op::Merge`
  pass-throughs. So unroll first runs **`collapse_trivial_merges`** (forwards
  single-input Merges to their input) and then accepts a `Region` *or* a
  back-edge `Merge` as a header, identifying entry vs back-edge by control-
  reachability (`forward_control_closure`). (LICM, which only looks for
  `Op::Region`, does not yet fire on these real loops — a known follow-up.)
- **Soundness** (the per-iteration set is exactly the loop-carried work): the
  clone set is the variant nodes backward-reachable from each carried phi's
  back-edge value and the loop condition — this *excludes* post-loop uses. A
  safety check requires every clone-set node's users to be other clone-set
  nodes, a carried phi, or the header `If` (results escape only via the phis we
  redirect). Any Store/Call/alloc/ArrayLength/Guard in the loop, an invariant
  load pinned to the header, an internal branch, or a non-constant trip ⇒ bail.
- **Diagnostics**: `CRATONVM_DBG_UNROLL` logs candidate headers, the per-region
  bail reason, and each successful unroll.

**Tests** — `cargo test -p cratonvm-jit unroll`: a counted reduction folds to
the known constant (sum 0..5 == 10), trip-cap respected (100 ⇒ no unroll),
non-zero init/stride (3,5,7,9 == 24).

**Live validation** (debug binary, real JDK 25, vs HotSpot): `UnrollProbe`
(trip-6 `i*i-1` reduction) and `UnrollProbe2` (four loops — strides +1/+2/-1,
`Lt`/`Le`/`Gt`, add/mul/sub reductions, trips 5/6/8) both produce **identical
checksums to HotSpot** with `CRATONVM_JIT_UNROLL=1`, and `[DBG_UNROLL]` confirms
the loops were actually unrolled. Default (flag off) is byte-for-byte unchanged.

**Next**: partial unrolling (unroll-by-factor with a remainder) for large/
non-constant trips; unrolling loops with internal branches (clone control);
re-using the new node-clone approach to make LICM fire on `Merge` headers.

## Increment 4 (DSE load-alias refinement) landed

Status: **landed** on `dev`. A precision improvement to the DSE overwrite phase
(Front 2.1) — the first slice of a real (if minimal) alias oracle.

**What landed** (`jit/src/ir_optimize.rs`, `eliminate_dead_stores`):

- The straight-line overwrite scan previously treated **every** `Op::Load` as an
  unconditional memory barrier (flushing all pending stores). It now resolves a
  load against the pending set with **allocation-level alias precision**:
  - Every pending store targets a local `New`/`NewArray` allocation, and two
    distinct local allocations never alias (the same fact the `(base, idx, kind)`
    location key already relies on). So a load whose base is a **different local
    allocation** flushes only pending stores to *its own* base, leaving stores to
    other local objects dead-eligible.
  - A load from a **non-local / unreadable base** (a `Param`, a `Call`/`Load`
    result, a phi, …) may alias any escaped local and still flushes everything.
- **Soundness**: the only way to read allocation `A`'s field is a load whose base
  resolves to `A` (which flushes `A`'s pending stores) or to a non-local node
  such as a load/phi result (which flushes all). So a store can only be removed
  as overwritten when no load between it and its overwrite could observe `A`. The
  shared `is_memory_barrier` helper still classifies `Load` as a barrier as the
  safe fallback should the explicit load arm ever be removed.

**Tests added** — `cargo test -p cratonvm-jit dse`:
- `test_dse_removes_overwrite_across_unrelated_local_load` — `store A; load B
  (distinct local); store A` ⇒ the first store to `A` is removed.
- `test_dse_keeps_overwrite_across_nonlocal_load` — `store A; load P (Param);
  store A` ⇒ the first store to `A` is kept (conservative full barrier).
- All six pre-existing DSE tests still pass (the same-object-load and
  distinct-fields cases were already exercising the same-base flush path).

**Not yet done**: a general alias oracle that also lets a load past a
*non-aliasing* in-loop store drive LICM hoisting (Front 2.2), and array-element
overwrite DSE (still gated on a real array `Store` operand layout).

## Increment 5 (LICM fires on javac `Merge`-header loops) landed

Status: **landed** on `dev`, behind the existing default-OFF `CRATONVM_JIT_LICM`
soak flag. Closes the increment-2/3 follow-up "make LICM fire on `Merge`
headers": until now LICM only recognised `Op::Region` loop headers (the
hand-built / EA-bridge shape), so it **never fired on real code** — the
production bytecode→IR builder emits javac loops as a back-edge `Op::Merge`
(the same shape the unroll pass already handles).

**What landed** (`jit/src/ir_optimize.rs`):

- **Generalised loop discovery (`loop_headers`)** replaces the
  `Region`-only `loop_regions`. A header is an `Op::Region` *or* a back-edge
  `Op::Merge`; of its control inputs, the back-edge(s) are control-reachable
  forward from the header itself (`forward_control_closure`) and the remaining
  input is the pre-header. Resolving entry vs back-edge **structurally** — not by
  assuming the entry sits at input slot 0 — is essential because the builder does
  not fix a `Merge` header's slot order (mirrors the unroll pass's discovery).
  Only reducible single-entry loops (exactly one pre-header) are returned.
- **`licm` now runs `collapse_trivial_merges` first** (the same normalisation
  unroll uses, folding the single-input `Merge` control pass-throughs the builder
  wraps around branch projections) so real loop headers/back-edges are
  recognisable, and uses the **classified** entry-predecessor as the pre-header.
- **`loop_body` takes the classified `(entry_pred, back_ctrls)`** and its forward
  control walk no longer stops at nested headers: an over-large forward set is
  trimmed back to the natural loop by the backward-reachability intersection, so
  a nested loop's control (and its barriers) stays *inside* the body. This is the
  safe direction — over-approximating the body only loses hoists, whereas
  under-approximating could hide a nested-loop store from the barrier check.
  (Net: also tightens the pre-existing nested-`Region` handling.)

**Soundness**: unchanged hoist criteria — only an `Op::Load` whose base and
address are loop-invariant, in a body with **no** memory barrier, is re-anchored
to the pre-header. The new code only changes which loops are *discovered* and how
entry/back-edge are identified; misclassification is prevented by the same
forward-reachability test the validated unroll pass uses.

**Tests added** — `cargo test -p cratonvm-jit licm`:
- `test_licm_hoists_invariant_load_merge_header` — an invariant load in a
  barrier-free `Merge`-header loop is hoisted to the pre-header.
- `test_licm_merge_header_classifies_entry_regardless_of_slot_order` — with the
  back-edge moved to input slot 0, LICM still hoists to the structurally-resolved
  entry (never the back-edge), proving slot-order independence.
- All four pre-existing `Region`-header LICM tests still pass.

**Boundary**: like the increment-1/2/4 escape/DSE work, LICM's load *hoisting*
only fires once the IR builder emits `Op::Load` (it currently bails on
field/array load opcodes, so production IR has no loads to hoist yet). What this
increment delivers now is correct **loop recognition** on the real `Merge`-header
shape — the prerequisite for every loop optimisation on real code, and already
proven to reach the IR path live by the unroll pass (increment 3). A general
alias oracle (hoist past a non-aliasing in-loop store) is increment 6.

## Increment 6 (LICM minimal alias oracle — hoist past non-aliasing stores) landed

Status: **landed** on `dev`, behind the default-OFF `CRATONVM_JIT_LICM` soak
flag. Closes the recurring increment-2/4/5 follow-up "let a load hoist past a
*non-aliasing* in-loop store". Composes the increment-4 DSE alias insight
("distinct local allocations never alias") with the increment-5 loop discovery.

**What landed** (`jit/src/ir_optimize.rs`): LICM no longer bails the whole loop
on *any* in-body memory effect. The barrier check is split:

- **`loop_has_hard_barrier`** (replaces the all-or-nothing `loop_has_memory_
  barrier`): a `Call`, allocation (`New`/`NewArray`, constructor side effects),
  guard, or monitor still disqualifies ALL hoisting — these may touch arbitrary
  memory. An in-loop `Store` is *not* a hard barrier.
- **`load_safe_past_loop_stores`** (per-load alias gate): a candidate invariant
  load is hoisted past the in-loop stores only when the load base AND every
  in-loop store base are **distinct local allocations**. A store writes only the
  memory of the object it names, so a store to a different `New`/`NewArray`
  leaves the load's object untouched (regardless of escape). Any non-local /
  unreadable / same base keeps the load pinned (conservative). Loops with no
  store at all behave exactly as before.

**Soundness**: the only relaxation is per-load and rests entirely on the
distinct-`New`-nodes-don't-alias invariant (the same one DSE increment 4 uses);
a wrong hoist past an aliasing store would be a stale-read miscompile, so the
oracle bails on anything it cannot prove distinct.

**Tests added** — `cargo test -p cratonvm-jit licm`:
- `test_licm_hoists_load_past_nonaliasing_local_store` — load of `A.f` hoists
  past an in-loop store to a distinct allocation `B.f`.
- `test_licm_keeps_load_when_store_to_same_alloc` — store to the *same* `A`
  keeps the load pinned (may alias).
- `test_licm_keeps_load_when_store_base_non_local` — store through a `Param`
  base keeps the load pinned (not provably non-aliasing).
- The pre-existing `test_licm_bails_on_barrier_in_loop` still bails — its in-body
  `New` is now the hard barrier. Full jit suite 784/784.

**Boundary**: same latency as increments 1/2/4/5 — the oracle only fires once the
IR builder emits `Op::Load`/`Op::Store` (production IR has neither yet).
Increment 7 extends the oracle past the local-alloc-only case.

## Increment 7 (alias oracle: store-to-alloc cannot alias a parameter load) landed

Status: **landed** on `dev`, behind the default-OFF `CRATONVM_JIT_LICM` flag.
Widens the increment-6 LICM alias oracle by one provably-sound class.

**What landed** (`jit/src/ir_optimize.rs`, `load_safe_past_loop_stores`): the
hoistable **load** base is no longer restricted to a local allocation — it may
now also be a **method parameter** (`Op::Param`, including `this`). The reasoning:
a `New`/`NewArray` executed in this method produces a reference that is *never*
an already-existing object, so it can never equal a parameter the caller passed
in (object identity is fixed at allocation, and this holds even if the
allocation later escapes). Hence a store to a fresh local allocation leaves any
parameter's memory untouched, and a load from a parameter may hoist past it.

**Asymmetry (deliberate)**: the **store** base is *not* widened to `Param`. Two
distinct parameters can be the same object (`foo(x, x)`), so a store through a
parameter is not provably non-aliasing — every in-loop store must still write a
distinct local `New`/`NewArray`. The widening is load-side only.

**Tests added** — `cargo test -p cratonvm-jit licm`:
- `test_licm_hoists_param_load_past_local_store` — a load of `P.f` (parameter)
  hoists past an in-loop store to a local allocation `B.f`.
- `test_licm_keeps_param_load_when_store_base_is_param` — a load of `P0.f` does
  NOT hoist past a store through `P1` (parameters may alias), locking the
  store-side asymmetry.
- All prior LICM/DSE/escape tests still pass. Full jit suite 786/786.

**Next**: increment 8 adds cross-merge points-to.

## Increment 8 (alias oracle: cross-merge points-to through phis) landed

Status: **landed** on `dev`, behind the default-OFF `CRATONVM_JIT_LICM` flag.
Generalises the increment-6/7 LICM alias oracle from direct `New`/`Param` bases
to bases that flow through a `Phi` (the IR's cross-merge primitive — Java
`select`/ternary lower to branch + phi, not a select op).

**What landed** (`jit/src/ir_optimize.rs`): the ad-hoc per-base checks were
replaced by a small points-to lattice that the oracle resolves for every load
and store base:

- **`resolve_ref_points_to`** maps a reference node to `{ allocs: Option<set>,
  has_pre: bool }`: a `New`/`NewArray` → that fresh allocation; a `Param` →
  pre-existing (no allocation); a `Phi` → the join of its value inputs (alloc
  sets union, `has_pre` ORs, any unknown input poisons `allocs` to `None`); a
  loop-carried phi cycle bottoms out at a depth bound → unknown. Anything else →
  unknown.
- **`loop_store_clobber`** summarises the loop's stores once: the union of
  allocations they may write, or `None` if any store is *opaque* (base resolves
  to a pre-existing or unknown value — could alias anything), which blocks all
  hoisting. A store via `(cond ? A : B).f` now resolves to writing `{A, B}`
  instead of bailing because the base is not a direct `New`.
- **`load_safe_past_clobber`** hoists a load when its possible-allocation set is
  disjoint from the clobber set (its pre-existing/parameter component is always
  safe — a fresh allocation is never an already-existing object).

This **subsumes increments 6 and 7 exactly** (a direct `New` base resolves to a
singleton set; a `Param` to the empty-set/pre-existing case) and adds the
phi-base case. The store base is still NOT widened to `Param` (two parameters can
be the same object); only definite-alloc-set stores are tame.

**Tests added** — `cargo test -p cratonvm-jit licm`:
- `test_licm_hoists_load_past_phi_store_of_distinct_allocs` — a store through a
  phi merging `{A, B}` does not block a load of a disjoint allocation `C`.
- `test_licm_keeps_load_when_phi_store_includes_its_alloc` — a load of `A` stays
  pinned past a phi-store that writes `{A, B}` (A is in the set).
- All six prior inc-6/7 alias tests still pass unchanged. Full jit suite 788/788.

**Next**: increment 9 admits a non-loop-carried invariant phi so phi *load*
bases hoist.

## Increment 9 (invariant phi recognition — cross-merge on the load side) landed

Status: **landed** on `dev`, behind the default-OFF `CRATONVM_JIT_LICM` flag.
Completes the cross-merge story by admitting an invariant `Phi` as a hoistable
load base — the increment-8 oracle could already resolve a phi *store* base, but
`is_loop_invariant` rejected *every* phi, so a phi *load* base never reached it.

**What landed** (`jit/src/ir_optimize.rs`, `is_loop_invariant_d`): the blanket
`Op::Phi => false` is replaced by a sound test — a phi is loop-invariant iff:

- its **control anchor** (input slot 0, the merge point) is OUTSIDE the loop
  body — so the merge is decided once, before the loop, not per iteration — AND
- every **value input** (slots 1..) is itself loop-invariant.

A phi anchored at this loop's region (induction / loop-carried) or at an in-loop
merge (an in-loop `if`/`else` join) has its anchor IN the body and stays variant,
exactly as before. Recursion is depth-bounded, so a self-referential value input
bottoms out as variant. This makes the `x = cond ? new A() : new B(); for (…) …
x.f …` shape hoist its `x.f` load: `x` is a pre-loop invariant phi, and the
increment-8 points-to resolver already understands `{A, B}` for the alias check.

**Tests added** — `cargo test -p cratonvm-jit licm`:
- `test_licm_hoists_load_with_invariant_phi_base` — a load over a phi merged
  before the loop (anchor outside the body) over invariant allocations hoists.
- The existing `test_licm_does_not_hoist_variant_load` still passes and now
  exercises the region-anchored-phi → variant path under the new logic.
- Full jit suite 789/789.

**Next**: treat a `final`/effectively-immutable field load as invariant
regardless of in-loop stores to *other* fields of the same object (a field-
sensitive refinement of the alias oracle).

## Increment 10 (step 2 — IR-vs-single-pass differential self-check harness) landed

Status: **landed** on `dev`. This is the ordered **step 2** ("differential
self-check harness, required before any gate relaxation"), built now because the
per-call `optimize` toggle (Step 3 of wire-tiered-manager) makes it directly
expressible.

**What landed** (`jit/tests/ir_vs_singlepass.rs`, + `cratonvm-types` added to the
jit crate's `[dev-dependencies]`): a corpus of pure-integer methods is compiled
through BOTH backends — `try_compile(.., optimize=true)` (optimizing IR pipeline)
and `optimize=false` (single-pass `x64`) — then **executed** via `try_call` and
asserted to return identical results, plus a host-computed correctness anchor.
The two backends are independent code generators for the same bytecode; any
divergence is a miscompile, and the gate must not be relaxed onto a method shape
until they agree on it.

Three corpus shapes pass (IR == single-pass == host): `add` (straight-line
arithmetic), `poly` (multi-op `a*a - 2*a + 1`), and `sum` (a counted loop with a
single exit).

**The harness immediately found a real IR miscompile.** A method with a
*conditional early return* (more than one `ireturn` point) — `int sgn2(int a) {
if (a<0) return -1; return 1; }` — is mislowered by the IR pipeline: for `a<0` it
drops the conditional branch and returns the fall-through value (`1`) instead of
`-1`; the single-pass backend is correct. The `sum` loop (which branches but has
a single exit) compiles fine, so the fault is specific to multiple return points,
NOT branching. This is captured as the `#[ignore]`d
`ir_vs_singlepass_conditional_early_return_known_divergence` test (run with
`-- --ignored` to reproduce). **It is a hard blocker for step 3**: the IR gate
must not be relaxed onto conditional-early-return methods until the multi-return
lowering is fixed. (Note: the φ/branch SIGSEGV fix proved single-return branchy
*expressions* like `(m & K) != 0`; multi-`return` control flow was not covered.)

**Gotcha recorded**: `CachedBytecodeMethod.code` is the bytecode **padded with
two trailing `0x00` bytes** — `jit::try_compile` strips them (`code.len() - 2`);
an unpadded corpus method silently drops its last two opcodes and emits without a
`ret`. The harness pads accordingly.

## Increment 11 (step 1 residual — IR multi-return / conditional-early-return fix) landed

Status: **landed** on `dev`. Fixes the miscompile increment 10's harness caught,
clearing the step-3 blocker.

**Root cause** (two parts): the IR builder emits one `Op::Return` terminator per
`ireturn`, but `graph.exit` records only the *last* one (each `ireturn`
overwrites it, `jit/src/ir.rs`). `eliminate_dead_nodes` (`jit/src/ir_optimize.rs`)
then rooted DCE **solely from `graph.exit`**, so every *other* return path — its
control, its value, and the `If`'s opposite projection — was marked unreachable
and deleted. The conditional collapsed into a single-successor `If` that always
took the surviving (last) return, so `if (a<0) return -1; return 1;` returned `1`
for every input.

**Fix**: `eliminate_dead_nodes` now seeds its worklist from **every** `Op::Return`
node, not just `graph.exit` (with a `graph.exit` fallback if a graph somehow has
none). Every return is an observable program exit and must be a DCE root. This is
strictly corrective — single-return methods are unchanged (their only return *is*
`graph.exit`), and genuinely-dead nodes (reachable from no return) are still
removed. The `ireturn` builder comment now documents that `graph.exit` is "an
exit", not the sole exit.

**Tests**:
- `jit/src/ir_optimize.rs::test_dce_keeps_all_return_paths` — a two-`Return`
  graph keeps both return paths through DCE (the first one survives even though
  `graph.exit` points at the second).
- `jit/tests/ir_vs_singlepass.rs` — the formerly-`#[ignore]`d divergence test is
  un-ignored and now passes, joined by `two_branch_three_returns` (`sgn3`) and
  `abs_early_return` (a computed early return). IR == single-pass == host for all.

**Unblocks step 3**: the IR gate may now be relaxed onto conditional-early-return
methods (behind a `CRATONVM_JIT_IR_*` soak flag), gated by this harness.

## Increment 12 (step 3 — IR gate relaxation: i2b / i2c / i2s) landed

Status: **landed** on `dev`. First slice of step 3 (relax the IR gate), validated
by the increment-10 harness.

The IR builder previously **bailed** (`build()` → `None` → fell to single-pass)
on the int-truncation conversions `i2b` (0x91), `i2c` (0x92), `i2s` (0x93), so
any byte/char/short-truncating int method never reached the optimizing IR path.
They now lower (`jit/src/ir.rs`), decomposed to existing ops — no new IR node or
lowering needed:

- `i2b` → `(x << 24) >> 24` (32-bit `SHL`/`SAR EAX`; the arithmetic shift
  sign-extends the low byte),
- `i2s` → `(x << 16) >> 16`,
- `i2c` → `x & 0xFFFF` (char is unsigned 16-bit).

The two bytecode length walkers (`find_branch_targets`, the loop-header walk)
list `0x91..=0x93` explicitly (they were already 1-byte via the default arm).

**Tests** (`jit/tests/ir_vs_singlepass.rs`): `i2b`, `i2c`, `i2s`, and
`i2b_chained` (`(byte)a + 1000`, proving the truncated value feeds a following
int op correctly). IR == single-pass == host across sign/zero-extension edge
cases (e.g. `i2b(128) = -128`, `i2c(-1) = 65535`, `i2s(32768) = -32768`). jit lib
790/790, harness 10/10.

**Why this scope**: the harness executes the compiled code with *dummy* runtime
helpers, so it can only validate helper-free shapes (pure arithmetic / branches /
loops / conversions). Relaxing the gate further onto methods with `invoke*` /
field / array ops needs (a) the IR builder to *emit* those ops (it bails today —
which is also why the DSE/escape/LICM passes are still latent) and (b) the
harness to supply real helpers or move to VM-level differential validation. Those
are the next step-3 slices.

## Increment 13 (step 3 — IR gate relaxation: tableswitch / lookupswitch) landed

Status: **landed** on `dev`. Second slice of step 3, harness-validated.

The IR builder bailed on `tableswitch` (0xaa) / `lookupswitch` (0xab) — so int
`switch` methods (state machines, dispatch) never reached the optimizing IR path.
They now lower as a **CMP-equality chain** (the same shape the single-pass
backend emits): each case becomes `if (key == match) goto target`, the unmatched
edge falls through to the next comparison, and the final unmatched edge goes to
the default. This reuses the existing `If`/`Cmp`/merge machinery — no dedicated
multi-way node or new lowering.

Implementation (`jit/src/ir.rs`):
- A shared `parse_switch` helper parses either table (4-byte padding, signed
  offsets relative to the opcode pc) and returns `(len, default_target, cases)`,
  reusing the single-pass `checked_tableswitch_count` / `checked_lookupswitch_
  npairs` caps and validating every target is in range (else `None` → bail).
- The build loop's `0xaa | 0xab` arm pops the key and emits the comparison chain.
- Both bytecode length walkers (`find_branch_targets`, `find_loop_headers`) use
  `parse_switch` to register every case + default target (a backward target is a
  loop header) and to advance the pc by the variable instruction length — without
  this they would mis-parse the switch table as opcodes.

**Tests** (`jit/tests/ir_vs_singlepass.rs`): `tableswitch` (dense 0..2 + default)
and `lookupswitch` (sparse keys 10/20 + default), each checked on hits and
out-of-range keys. IR == single-pass == host. jit lib 790/790, harness 12/12.

## Increment 14 (step 3 — IR gate relaxation: int-category `getfield` → `Op::Load`) landed

Status: **landed** on `dev`. **First slice of "THE NEXT FRONTIER"** (field / call /
alloc emission). The IR builder now lowers an int-category `getfield` into the
first real `Op::Load` the production IR path emits — so a method whose only heap
op is an int-field read takes the optimizing IR pipeline, where before it bailed
to single-pass.

**Scope — read-only, sound without a memory scheduler.** Only `getfield` of an
int-category field (`I`/`Z`/`B`/`C`/`S`) lowers. Crucially this needs **no**
scheduler memory-ordering work: a getfield-only method has no `Store`/`Call`, so
there is nothing for the (still memory-unaware) scheduler to mis-order against —
loads of immutable memory may freely float / GVN / DCE. `putfield`, `new`, array
ops, `invoke*`, and float/long/double/reference fields all still bail (`build()`
→ `None` → single-pass), the existing safety net. (Writes need real scheduler
memory ordering + lowerer helper access — a separate slice.)

**Implementation**
- **`jit/src/ir.rs`** — the builder gained `set_field_info(pc → (field_index,
  type_tag))` (an `IrBuilder` field set by the caller before `build`; absent for
  hand-built/test graphs). The `getfield` (0xb4) arm looks up the pc, bails on an
  unresolved or non-int-category field, then emits
  `Op::Load(MemKind::Int)` with inputs `[ctrl, mem, base, Const(field_index)]`
  (the offset operand is the field index as a `Const`, keeping it visible to a
  future field-sensitive alias oracle). `aload`/`aload_0..3` were added (a
  getfield base is just a `NodeId` on the abstract stack). Both bytecode length
  walkers list `0xb4` (3-byte), `0x19` (2-byte), `0x2a..=0x2d` (1-byte) — without
  this a branchy/looping getfield method mis-parses the field index as opcodes.
- **`jit/src/ir_lower.rs`** — a new `Op::Load(_)` arm emits the single-pass inline
  getfield ABI byte-faithfully: receiver → RAX, `TEST/JE` null guard (null → 0,
  matching `jit_getfield`'s early return), else `MOVSXD RAX, [RAX +
  HEADER_SIZE + field_index*SLOT_SIZE + FIELD_CELL_PAYLOAD32_OFFSET]`
  (sign-extend the 32-bit `Value::Int` payload). Constants come from
  `cratonvm_types` (same source the single-pass backend uses).
- **`jit/src/lib.rs`** — the IR branch builds the `pc → (field_index, type_tag)`
  map from `scan.field_ops` + `cp_field_resolver` and calls `set_field_info`
  before `build`; an unresolved field is omitted → that getfield bails.

**Tests**
- `jit/tests/ir_vs_singlepass.rs` — the harness gained a synthetic-heap-object
  builder (`make_object`, VM-faithful header + 16-byte `Value` cells) and a field
  resolver, then **executes** four getfield methods through both backends against
  real objects: `getfield_simple` (getter), `getfield_two_fields_sum`,
  `getfield_branch` (field read feeding a conditional + two returns), and
  `getfield_loop` (getfield in both the loop condition and body, re-read each
  iteration under loop-carried + memory phis). IR == single-pass == host for all.
- `jit/src/ir.rs` — `test_ir_getfield_emits_load` (Load with the right base/offset
  operands) + bail tests (no field info; non-int field).
- `jit/src/lib.rs` — `step3_getfield_int_routes_through_ir` proves via
  `IR_LOWER_COMPILES` that an int getfield method actually takes the IR pipeline
  (not a vacuous single-pass fall-through), and bails without a resolver.

jit lib 794/794, harness 16/16, `cratonvm-vm` builds clean.

**Next** (the rest of the frontier): `putfield` → `Op::Store` (needs scheduler
memory ordering so a store can't be re-ordered past an aliasing load, plus a
lowerer helper-call path or an inline tag+payload write), then `new` / `Op::Call`
— which is also what finally de-latents the inc-4–9 DSE/escape/LICM passes on
production IR (they only fire once `Store`/`New` exist).

## Increment 15 (step 3 — IR gate relaxation: int-category `putfield` → `Op::Store`) landed

Status: **landed** on `dev`. Second slice of the field/call frontier: an
int-category `putfield` now lowers to the first real `Op::Store` the production
IR path emits. Together with inc 14 (`Op::Load`) the IR pipeline now does both
reads and writes of int fields.

**Memory ordering — the keystone, solved without a new scheduler.** The IR
threads a single memory-token chain: *every* memory op consumes the current
token and produces a new one. A `getfield` `Op::Load` now also advances the
token (`self.mem = load`), and a `putfield` `Op::Store` consumes the prior token
and becomes the new one. Because the scheduler's within-block order is a
post-order DFS over **input edges** (`ir_schedule::topo_sort_block`), this chain
is exactly the dependency that serialises memory ops in program order — RAW
(load sees a prior store), WAR (a store waits for a prior load of a possibly-
aliasing location), and WAW are all preserved with no memory-aware scheduler
pass. (No alias precision yet: the chain is total, conservatively serialising
even provably-independent accesses; the inc-4–9 alias oracle can refine this
later.)

**Implementation**
- **`jit/src/ir.rs`** — `putfield` (0xb5) emits `Op::Store(MemKind::Int)` with
  inputs `[ctrl, mem, base, Const(field_index), value]`, typed `IrType::Memory`,
  and sets `self.mem = store`; the `getfield` `Op::Load` now also advances
  `self.mem`. `0xb5` added to both length walkers. Non-int fields / unresolved
  layout bail.
- **`jit/src/ir_optimize.rs`** — `eliminate_dead_nodes` now roots from every
  `Op::Store` as well as every `Op::Return`. A store is an observable side
  effect whose memory-token result may be consumed by no one (a pure-write
  `o.x = v; return v;`), so rooting only from returns would delete it. Strictly
  additive (a DSE-removed store is already `Op::Dead`).
- **`jit/src/ir_lower.rs`** — an `Op::Store(_)` arm inlines the
  `jit_putfield_int` heap write: null receiver → no-op (matching the helper),
  else write a `Value::Int` cell (discriminant 0 + 32-bit payload, high qword
  cleared so no stale ref survives — the scalar-replace precedent). The receiver
  and value are loaded before the null check so the guarded body is a fixed 27
  bytes (a constant `JE` displacement). Produces no value, so no slot is
  allocated.

**Tests** — `jit/tests/ir_vs_singlepass.rs` gains a `putfield_int` stub (so the
single-pass backend, which lowers an int putfield to `CALL jit_putfield_int`, can
execute) and a read/write differential that runs **each backend against its own
fresh object** and compares the return value AND the post-call object state:
`putfield_then_getfield` (RAW), `getfield_then_putfield` (WAR — proves the store
waits for the read), `putfield_pure_write` (the store's memory result is unused —
proves DCE keeps it), and `putfield_two_fields` (WAW + two fields). IR ==
single-pass == host for all. `jit/src/ir.rs` adds builder tests for store
emission and for the store's memory input being the prior load.

**Trap recorded** (cost an investigation): a single-pass `putfield` method is
`needs_heap` (`x64.rs` sets it unconditionally for 0xb5, since a *ref* putfield
needs the VM pointer for write barriers), which makes the compiled body
`needs_context` — it takes a hidden VM-context pointer as its first argument. The
differential harness must invoke single-pass putfield via
`try_call_with_context(dummy, [obj, …])`, not `try_call([obj, …])`, or the
receiver lands in the context slot and every arg shifts by one (manifested as a
`STATUS_ACCESS_VIOLATION` writing through `obj == value`). The inline IR store
needs no context (`needs_context() == false`), so the harness dispatches on
`needs_context()`. The int putfield path never dereferences the context pointer,
so a zeroed dummy buffer suffices.

jit lib 796/796, harness 20/20, `cratonvm-vm` builds clean.

**Next**: `new` / `Op::Call` emission (needs the lowerer to gain
`JitRuntimeHelpers` access for the allocation/dispatch helper calls), which also
finally de-latents the inc-4–9 DSE/escape/LICM passes on production IR.

## Increment 16 (Front 3 — EA bridge handles the full-layout Load/Store) landed

Status: **landed** on `dev`. The foundational first piece of the `new`/`Op::Call`
frontier: the IR→escape-analysis bridge now correctly translates the
**production** field-access layout, which is the prerequisite for escape analysis
to ever scalar-replace a non-escaping allocation on real IR.

**The bug it fixes.** `escape_analysis_from_ir` (`lib.rs`) used to copy an IR
node's inputs verbatim into the EA graph. But the EA graph reads memory operands
in a **compact** layout (`Store [holder, value]`, `Load [holder]`) keyed by a
real **field index**, while the production `Op::Load`/`Op::Store` the builder now
emits (inc 14/15) are **full-layout** (`[ctrl, mem, base, offset, value]`)
carrying a `MemKind`. Forwarded verbatim, EA read the *holder* from input[0] (the
control edge) and the field index from the `MemKind` discriminant (always
`Int`=0). So EA could never match a field store/load to its allocation → it never
scalar-replaced anything on real IR (silently conservative, hence sound but
inert).

**What landed** (`jit/src/lib.rs`):
- `ir_load_store_field_index` recovers the real field index from the `Const`
  offset operand (input[3]); `escape_analysis_from_ir`'s first pass uses it for
  the EA `Load`/`Store` op, and the second pass emits the compact operands
  (`holder = input[2]`, `value = input[4]`). Non-Load/Store nodes are forwarded
  verbatim; a malformed/compact node yields empty operands EA treats
  conservatively.
- `apply_ea_to_ir` now derives the load's field index the same way (real index,
  `MemKind` fallback) so its `field_values` lookup agrees with the bridge.

**Why this is sound and inert today**: the only full-layout Load/Store in
production come from int getfield/putfield whose base is a `Param` (escaping), so
EA still finds nothing scalar-replaceable there — no behaviour change. The fix
only *enables* scalar replacement for the not-yet-emitted `Op::New` case.

**Tests** (`jit/src/lib.rs`): `ea_bridge_scalar_replaces_full_layout_new_store_
load` builds a by-hand `o = new Foo(); o.f1 = 42; return o.f1` graph in the
production layout (field index 1, distinct from `MemKind::Int`=0) and asserts EA
kills the New/Store/Load and redirects the return to the stored `Const(42)`;
`ea_bridge_keeps_escaping_new` asserts a returned-by-reference New is NOT
scalar-replaced (the kafka bug-25 escape rule is preserved). jit lib 798/798,
field harness 20/20.

**Remaining for the `new` scalar-replacement slice**: see increment 17 (the
mechanism) below.

## Increment 17 (Front 3 — `Op::New` emission + scalar-replacement mechanism) landed

Status: **landed** on `dev`; **inert in production** (not yet wired — see the
soundness analysis). The IR builder can now lower `new` to `Op::New` and the EA
scalar-replaces a non-escaping allocation end-to-end, but the production caller
does not yet feed the builder the allocation metadata, because eliding a
constructor soundly needs a signal that does not yet exist.

**What landed** (`jit/src/ir.rs`, `jit/src/lib.rs`):
- The builder gained `set_new_info(pc → (class_id, num_fields), trivial_init_pcs)`.
  `new` (0xbb) emits `Op::New { class_id, num_fields }` (inputs `[ctrl, mem]`);
  `invokespecial` (0xb7) is **elided** iff its pc is in `trivial_init_pcs` AND
  the receiver on the abstract stack is a fresh `Op::New` we emitted (defence in
  depth — eliding a `<init>` on `this`/a parameter would skip a real superclass
  constructor and hide any escape it performs). Any other `invokespecial`, or a
  `new`/`<init>` without resolved metadata, bails to single-pass. `0xbb`/`0xb7`
  added to both length walkers.
- **Surviving-New gate** (`lib.rs`): after escape analysis, if any `Op::New`/
  `Op::NewArray` is still live (it escaped → was not scalar-replaced), bail to
  single-pass — the lowerer has no allocation path, so emitting nothing for it
  would leave a garbage object reference. (Scalar-replaced News are `Op::Dead`.)

**Tests**: `ir_new_scalar_replaces_end_to_end` drives the whole path from
bytecode (`Foo o = new Foo(); o.x = 42; return o.x`): the builder emits the New
+ elides the `<init>`, `optimize` + EA + `apply_ea_to_ir` scalar-replace it, and
the return resolves to `Const(42)` with no live New. `ir_new_bails_on_init_of_
nonfresh_receiver` proves a `super.<init>()` on `this` bails. jit lib 800/800,
field harness 20/20 (no regression; the gate is inert with no News emitted).

**The `<init>`-soundness analysis (why production wiring is DEFERRED).** Eliding
`new Foo(); dup; invokespecial Foo.<init>` is sound only if `Foo.<init>` is
provably **effect-free and does not escape its receiver**. Three hazards, none
visible from the call site:
1. **Field initialisers** — `Foo(){ x = 5; }` is a `()V` `<init>` that sets a
   field; eliding it leaves the scalar slot at the zero default. (For int fields
   the builder only admits a `new` whose `has_primitive_init == false`, i.e. no
   non-zero primitive initialiser — but that is a *future* wiring constraint, and
   reference-field initialisers are irrelevant only because a non-escaping object's
   unread ref fields are dead.)
2. **Escape inside the constructor** — `Foo(){ GLOBAL.add(this); }` escapes the
   object *through the elided body*, which the caller's EA cannot see, so it would
   wrongly scalar-replace a live, escaped object (the kafka bug-25 class). The
   surviving-New gate does **not** catch this (the escape is hidden in the elided
   `<init>`).
3. **Arbitrary side effects** — a `()V` `<init>` may call other methods / do I/O.

Single-pass treats **any** `()V` `<init>` as trivial (`is_trivial_void_init`,
`x64.rs`) — an approximation that holds for its targeted patterns but is not
provably sound. The only `<init>` provably safe to elide from the descriptor/name
alone is `java/lang/Object.<init>()V` (empty), which scalar-replaces nothing
useful (no fields). **A sound *and* useful production policy needs either (a) a
VM-side "trivial constructor" signal — `<init>` only calls `super.<init>()` and
does zero/default field stores, no escape, no other call — added to
`cp_new_resolver`, or (b) constructor inlining so the `<init>` body's effects
become visible IR.** Until one lands, `set_new_info` stays unwired in production
(the mechanism is proven and ready; activating it on an unsound policy would
reintroduce exactly the miscompile class this project guards against).

**Next**: the VM-side trivial-constructor signal (smallest sound unlock) OR
`Op::Call` for real `invoke*` (needs the lowerer to gain `JitRuntimeHelpers`
access + VM-level differential validation per `wire-tiered-manager`).

## Increment 18 (Front 3 — `apply_ea_to_ir` zero-default for an un-stored field) landed

Status: **landed** on `dev`. A soundness prerequisite for activating scalar
replacement. `find_scalar_replacements` records `field_values[idx] = None` for a
field that is **loaded but never stored** (the design intent — "use the object's
zero default", per `escape_analysis.rs::test_uninitialized_field_returns_none`),
but `apply_ea_to_ir` only redirected a load when `field_values` was `Some` and
then killed the load **unconditionally** — so a load of an un-stored field was
marked `Dead` with **no replacement**, leaving its consumers reading a dead node.
Fix: when `field_values[idx]` is `None`, materialise a `Const(0)` (the correct
default for a zero-initialised object's int field) and redirect the load to it.
Sound only when the object is genuinely zero-initialised — which the eventual
production caller must enforce (only admit allocations whose constructor sets no
non-zero field). Test: `ea_unstored_field_load_resolves_to_zero_default`. jit lib
801/801, field harness 20/20.

## The VM-side trivial-constructor signal — actionable plan (the next sound unlock)

The `Op::New` mechanism (inc 17) + the EA bridge (inc 16) + the zero-default fix
(inc 18) are all in place; production scalar replacement is one signal away. The
signal must answer: *is it sound to elide `new C(); dup; invokespecial C.<init>()V`
and zero-initialise the scalar slots?*

**Do NOT reuse `classify_init_complexity`** (`vm/src/jit/skip_list.rs`). Its
`Trivial` means "no putfield/putstatic/monitor/invokedynamic" — sound for
JIT-*compiling* the `<init>`, but it **admits regular calls** (`invokevirtual`/
`invokestatic`/…). A `()V` ctor `C(){ register(this); }` is `Trivial` by that
classifier yet escapes the receiver — eliding it would scalar-replace a live,
escaped object (the surviving-New gate can't see the escape; it's hidden in the
elided body).

**Sound + simple + useful definition** — an *elidable construction*:
`C.<init>()V`'s body is exactly `aload_0; invokespecial java/lang/Object.<init>()V;
return` (bytes `2a b7 XX XX b1`, with `XX XX` resolving to `Object.<init>()V`).
That is the default empty constructor of a direct `Object` subclass — no field
stores (object stays zero-initialised → the inc-18 zero-default is correct), no
escape of `this`, no side effects. Covers the common POJO/data-class case
(`class Point { int x, y; }`). (A later refinement can recurse the super chain to
admit non-`Object` supers whose `<init>` is itself elidable.)

**Wiring** (cross-crate; production-activating → needs a soak):
1. **VM**: an `is_elidable_construction(class_id) -> bool` in
   `vm/src/runtime/interpreter.rs` near `resolve_jit_new_site` (it has CP +
   hierarchy access) that checks the `<init>()V` body shape + resolves the
   `invokespecial` target to `Object.<init>()V`.
2. **Thread it** as a 5th field of the `cp_new_resolver` tuple
   (`(class_id, num_fields, has_prim_init, has_finalizer, is_elidable)`) — the
   least-disruptive option (one closure signature, ~3 call sites: interpreter.rs,
   tiered.rs, the lib.rs consumer + the harness/in-crate test pass dummies).
3. **lib.rs** (IR branch): build `new_info` from `scan.new_ops` (all news), and
   `trivial_init_pcs` by linking each `new` (pc P, `is_elidable`) to the
   `invokespecial <init>` that consumes its receiver — the canonical
   `new@P; dup; invokespecial@P+? ` pair (match by the invoke immediately
   following the new+dup, or resolve the invoke's class == the new's class).
   Call `builder.set_new_info(new_info, trivial_init_pcs)`.
4. **Gate behind a default-OFF soak flag** (e.g. `CRATONVM_JIT_SCALAR_NEW`, like
   `CRATONVM_JIT_LICM`) so it lands inert and the production flip waits on a
   **bt18 (== 68332206) + gauntlet soak** — it changes production scalar
   replacement, the kafka-bug-25-sensitive area.
5. **Validation**: the differential harness already validates scalar replacement
   (a non-escaping `new` folds to pure-int → IR == single-pass == host); add a
   `new`-bearing case once the resolver is wired. The surviving-New gate (inc 17)
   + the zero-default (inc 18) + the receiver-is-New check (inc 17) are the
   safety net.

## Increment 19 (Front 3 — VM-side trivial-constructor signal wired, soak-gated) landed

Status: **landed** on `dev`, **default-OFF behind `CRATONVM_JIT_SCALAR_NEW`**.
Production scalar replacement of `new` is now fully wired end-to-end (VM analysis →
resolver → `lib.rs` → builder → EA), but stays inert until the soak flag is set,
because flipping it on changes production scalar replacement (the
kafka-bug-25-sensitive area) and must clear a bt18 + gauntlet soak first.

**What landed**
- **VM** (`vm/src/runtime/interpreter.rs`): `is_elidable_construction(cm,
  class_id)` — true iff the class's `<init>()V` body is exactly `aload_0;
  invokespecial java/lang/Object.<init>()V; return` (the empty default
  constructor of a direct `Object` subclass: no field initialiser → object stays
  zero-initialised, no escape of `this`, no side effect). `resolve_jit_elidable_
  init(cm, holder, invoke_cp_idx)` resolves an `invokespecial` methodref and
  applies that check. Deliberately stricter than `classify_init_complexity`'s
  `Trivial` (which admits calls that can escape the receiver — unsound to elide).
- **JIT** (`jit/src/lib.rs`): a new `try_compile` parameter
  `cp_elidable_init_resolver: Option<&dyn Fn(u16) -> bool>`. When supplied, the
  IR branch builds `new_info` (from `cp_new_resolver`) + `trivial_init_pcs` (the
  `invokespecial` pcs the resolver marks elidable) and calls
  `builder.set_new_info`. `None` (the default) leaves it off — the builder bails
  on `new`/`invokespecial`, single-pass as before.
- **VM call sites**: each of the three `try_compile` sites builds the elidable
  resolver and passes it **only when `CRATONVM_JIT_SCALAR_NEW` is set**, else
  `None`. So production is inert by default.

**Tests**: `scalar_new_wiring_routes_through_ir_only_with_resolver` (a
`new Foo(); o.x=42; return o.x` method routes through the IR pipeline —
`IR_LOWER_COMPILES==1` — only with the resolver; without it, counter stays 0).
jit lib 802/802, field harness 20/20, `cratonvm-vm` builds clean. (Combined with
the inc-17 end-to-end builder test proving the graph folds to `Const(42)` and the
inc-18 zero-default, the path is covered down to the machine-code level.)

**To soak / flip on** (the remaining production-validation step):
1. Run with `CRATONVM_JIT_SCALAR_NEW=1` on the app gauntlet (kafka / spring /
   tomcat / hibernate suites) + `bt18` (must stay `== 68332206`; bintrees' own
   `TreeNode(left,right)` ctor is arg-bearing so NOT elidable → bt18 only checks
   the flag-on path doesn't regress, it doesn't exercise scalar-new). A targeted
   probe (`new`-heavy default-ctor POJOs, non-escaping) exercises the new path —
   compare its output to HotSpot.
2. Watch for the kafka-bug-25 class: an object that escapes via an elided
   constructor body. The `Object.<init>`-only restriction makes the elided body
   provably empty, so this is structurally excluded — but the soak is the proof.
3. Once clean, default the flag on (or remove it) and re-run the gauntlet +
   bt10/14/16/18 checksums, per step 8.

**Next refinements** (after the flag flips clean):
- Recurse the super chain in `is_elidable_construction` to admit non-`Object`
  supers whose `<init>` is itself elidable (covers deeper hierarchies).
- `Op::Call` for real `invoke*` — the remaining big lever (lowerer needs
  `JitRuntimeHelpers` access + VM-level differential validation per
  `wire-tiered-manager`).

## Increment 20 (Front 3 — `astore` gap fixed + `new` scalar replacement default-ON) landed

Status: **landed** on `dev`. Closes Gap A of the scalar-new handoff, but the
headline is a **latent-bug fix**: `new` scalar replacement (inc 17–19) was
**completely inert on real bytecode** — it never fired once outside the unit
tests — and the soak that was supposed to prove it (inc 19's "POJO probe ==
HotSpot") was **vacuous**: a non-escaping POJO produces the same result whether
or not it is scalar-replaced, so "== HotSpot" passed while the optimization did
nothing.

**Root cause (the `astore` gap).** The IR builder (`jit/src/ir.rs`) lowered
`aload`/`aload_0..3` (read a reference local) but had **no `astore` handler**.
Real javac compiles `Foo o = new Foo()` as `new; dup; invokespecial <init>;
astore_N` — it stores the fresh object into a local. With no `astore` arm, the
builder hit its `_ => return None` catch-all on *every* allocation method and
bailed to single-pass — so `Op::New` was emitted, the `<init>` elided, but the
method never reached escape analysis. The inc-17 end-to-end unit test passed
only because it hand-builds bytecode that keeps the ref on the *stack* via `dup`
(`new; dup; invokespecial; dup; …`), never exercising `astore`. **Lesson: a
"== reference output" probe cannot validate an optimization whose presence is
output-invariant; assert the optimization *fired* (here via a
`CRATONVM_DBG_SCALAR_NEW` live-fire diagnostic), not just that the result
matches.**

**What landed**
- **`jit/src/ir.rs`** — the builder now lowers `astore` (0x3a) and
  `astore_0..3` (0x4b..=0x4e), mirroring `istore` exactly (a reference is just a
  `NodeId` slot in the abstract locals array, per the existing `aload` comment).
  Both bytecode length walkers (`find_branch_targets`, `find_loop_headers`) list
  `0x4b..=0x4e` (1-byte) and `0x3a` (2-byte). This widens the IR path generally
  (it also un-bails the already-default-on int `getfield`/`putfield` path when a
  ref base arrives via a local), not just scalar-new.
- **`vm/src/runtime/interpreter.rs`** — `CRATONVM_JIT_SCALAR_NEW` flipped from
  opt-in to **default-ON** at all three `try_compile` sites
  (`std::env::var(..).map_or(true, |v| v != "0")`); `CRATONVM_JIT_SCALAR_NEW=0`
  is the opt-out safety net (restores single-pass for `new`-bearing methods). The
  noisy debugging `is_elidable_construction` print was removed.
- **`jit/src/lib.rs`** — a focused `CRATONVM_DBG_SCALAR_NEW` diagnostic: for an
  allocation method it reports `scalar-replaced N/M alloc(s)` (proves the path is
  non-vacuously exercised) and flags an allocation method that bailed the IR
  builder (the signal that surfaced this very gap).

**Soundness of the widening**: `astore` itself is a trivial slot assignment; the
builder still bails (`None` → single-pass) on any opcode it can't lower, so the
newly-admitted methods are exactly those whose every op is already validated
(int arithmetic/branches/loops + int `getfield`/`putfield` + elidable-`new`).
The escape rules (Return→GlobalEscape, Call-arg→ArgEscape, store-value→bail) and
the surviving-`New` gate are unchanged.

**Tests**
- `jit/src/lib.rs::ir_new_scalar_replaces_through_astore_local` — the inc-17
  end-to-end fold, but through an `astore`/`aload` local (the real javac shape):
  `new Foo(); o.x=42; return o.x` folds to `Const(42)`. This is the regression
  guard the inc-17 dup-only test could not be.
- `jit/tests/ir_vs_singlepass.rs::ir_vs_singlepass_getfield_via_astore_local` —
  an `astore`/`aload` round-trip on the int-field path executes identically
  (IR == single-pass == host).

**Validation** (worktree `CratonVM-irnew`, branch `feat/ir-scalar-new-flip`,
binary `cratonvm-irnew.exe`):
- jit lib 803/803, differential harness 21/21; `cratonvm-vm` builds clean; clippy
  neutral (the 10 pre-existing jit clippy errors are all in `deopt.rs`/`x64.rs`,
  none in the changed files).
- bt10/14/16/18 == HotSpot (`135854 / 3222190 / 14985902 / 68332206`) with the
  flag default-ON **and** with `CRATONVM_JIT_SCALAR_NEW=0`; no timing regression
  (a controlled bt16 A/B vs the old dev binary was within noise — the new
  binary, opt-out, and old dev binary all ~6.9 s).
- `scratch/scalarnew/ScalarNew.java` (pure-int POJO probe: straight-line,
  loop, conditional-alloc, escaping-via-return) == HotSpot, **and** the
  `CRATONVM_DBG_SCALAR_NEW` diagnostic confirms `oneShot`/`sumPoints`/`sumBoxes`
  scalar-replace `1/1`, while the escaping helper correctly bails. (Pure-int is
  mandatory: a `long` accumulator makes the whole method category-2 →
  single-pass, which is what made the *original* probe vacuous twice over.)

**Next**: recurse the super chain in `is_elidable_construction` (deeper
hierarchies than direct-`Object` POJOs); `Op::Call` for real `invoke*`
(Gap B — the remaining big lever).

## Increment 21 (Gap B — `Op::Call` for int `invokestatic`, gated) landed

Status: **landed** on `dev`, **default-OFF behind `CRATONVM_JIT_IR_CALL`**. The
remaining big lever — the IR builder emits a real method call. This first slice
covers **`invokestatic` with int-only args + an int/void return, in an oop-free
method**, dispatched through the existing `jit_invoke_dispatch` helper (the same
ABI single-pass uses). It lands inert (gated off) + validated; flipping it on is
a soak follow-up (like inc 19→20 for scalar-new).

**The GC-safety insight that scopes the slice.** The IR path was GC-safe only
because it had **no calls and no real allocations → no safepoints → GC never
runs mid-method**, so the lowerer needs no oop maps (it has none). A call is a
safepoint (GC can run in the callee), so any object reference live across it
would need a GC root map. Rather than build oop maps, this slice restricts to a
**provably oop-free method**: no getfield/putfield (a ref receiver), no
getstatic/putstatic, no `new`, no array allocation, all parameters primitive,
and every invoke an int-only `invokestatic`. Then *no* object reference exists in
the frame at all, so a GC at the call has no roots here to find — sound without
an oop map. (Virtual/special/interface dispatch — inline caches — and
oop-across-call GC maps are the follow-ups.)

**What landed**
- **`jit/src/ir.rs`** — `Op::Call { info_ptr }` carries the leaked
  `JitInvokeInfo` address. The builder lowers `invokestatic` (0xb8) to `Op::Call`
  (inputs `[ctrl, mem, args…]`), pops the args, pushes the result for a non-void
  call, and threads the memory token (a call is a hard barrier — it consumes the
  prior token and becomes the new one, like `Op::Load`/`Store`). `0xb8` added to
  both length walkers. `set_invoke_info(pc → (info_ptr, num_args, returns_value))`
  is the wiring hook; an `invokestatic` pc not present bails to single-pass.
- **`jit/src/ir_lower.rs`** — the lowerer gained `JitRuntimeHelpers` access and an
  `Op::Call` arm that marshals the Java args into a frame staging region, sets the
  four helper register args `(vm_ptr, info_ptr, args_ptr, num_args)`, `CALL`s
  `invoke_dispatch`, and emits the `i64::MIN` exception sentinel check (`JE` →
  a shared bail stub that returns the sentinel so the VM takes the pending
  exception — the single-pass protocol). A method with an `Op::Call` is
  `needs_context`: the prologue takes the VM pointer in ABI[0] and shifts the
  Java params; `cm.needs_context` + `cm.has_dispatch` are set (the latter makes
  the VM wrap the call in `set_jit_thread` + `catch_unwind` and drain the pending
  exception). Bails (single-pass) if `1 + num_params` exceeds the ABI registers.
- **`jit/src/lib.rs`** — a new `try_compile` parameter `ir_emit_calls`. When on,
  the IR branch checks the oop-free gate (`descriptor_has_ref_params` +
  `static_call_int_shape` + empty field/static/new/array scans), builds the leaked
  `JitInvokeInfo` boxes for the invokestatic sites, calls `set_invoke_info`, and
  attaches the boxes/strings to the returned `CompiledMethod` (so the baked
  `info_ptr`s outlive the code) + sets `has_dispatch`. `CRATONVM_DBG_IR_CALL`
  reports the emitted-call count per method.
- **`vm/src/runtime/interpreter.rs`** — the 3 `try_compile` sites pass
  `ir_emit_calls` from `CRATONVM_JIT_IR_CALL` (default-OFF). No other VM change
  needed: the cache already populates `needs_heap` from `compiled.needs_heap()`,
  so an `Op::Call` method is correctly invoked via `try_call_with_context`.

**Tests**
- `jit/tests/ir_vs_singlepass.rs` — a real stub `invoke_dispatch` (the handoff's
  "direct static call" option) lets the IR-emitted call actually RUN:
  `invokestatic_two_int_args` (order/count/value-sensitive marshalling +
  `needs_context`), `invokestatic_three_args_and_arith` (3 args, result feeds
  arithmetic), `invokestatic_exception_sentinel` (`i64::MIN` → bail).
- `jit/src/lib.rs::ir_call_wiring_routes_through_ir_only_with_flag` — proves the
  IR path FIRES (`IR_LOWER_COMPILES==1`) only with `ir_emit_calls`, and bails
  (==0) without it. This guards against a **vacuous** validation: single-pass
  ALSO dispatches `invokestatic` correctly, so result-equality alone (the harness)
  would not prove the IR path ran — the inc-20 "vacuous soak" lesson applied.

**Validation**: jit lib 804/804, differential harness 24/24, `cratonvm-vm` builds
clean. Live probe `scratch/ircall/IrCall.java` (a hot oop-free method with three
int `invokestatic` calls in a loop) == HotSpot (`23762906400000`) with the gate
OFF **and** ON, and `CRATONVM_DBG_IR_CALL` confirms it emits 3 `Op::Call`s (the
path fires). bt10/14/18 == HotSpot gate-OFF and gate-ON; 4 bench programs
(`FieldCheck`/`IntegrationTest`/`GenPair`/`Benchmark`) == HotSpot gate-ON.

**To soak / flip on** (the remaining production-validation step, like inc 19→20):
1. Run `CRATONVM_JIT_IR_CALL=1` on the app gauntlet (kafka/spring/tomcat/
   hibernate) + bt10/14/16/18, watching for any dispatch/exception/GC divergence.
2. Flip the default (or remove the gate) once clean.

**Next refinements** (after the flag flips clean): see increment 22 (oop-free
restriction lifted), then `invokespecial`/virtual dispatch and category-2 args.

## Increment 22 (Gap B — oop-free restriction lifted: oops live across `Op::Call`) landed

Status: **landed** on `dev`, still under `CRATONVM_JIT_IR_CALL` (default-OFF).
Removes the inc-21 "oop-free method only" gate: an `invokestatic` `Op::Call` may
now have **reference parameters, reference call arguments, reference returns, and
int field ops** (a ref receiver) — i.e. object references *live across the call*.

**Why it's GC-sound without oop maps** (the key finding). The IR lowerer spills
every value to a frame slot — it keeps **no oop in a register across a call**
(unlike single-pass, the source of the A2/A3 register-root UAFs). `JitEntryGuard`
**conservatively scans** the IR frame's slots `[rsp, entry_sp)` at every
safepoint, and the GC is forced **non-moving while any JIT frame is active**
(`gc_quiescence`), so a pointer found in a slot is **pinned, never relocated** —
a false positive (an `i64` that looks like a pointer) is harmless, and a real
reference live across the call is neither moved nor reclaimed. No precise oop map
is required. (Single-pass needs precise maps because it keeps oops in registers;
the IR lowerer's spill-everything model is exactly what makes the conservative
scan sufficient here.)

**What landed**
- **`jit/src/lib.rs`** — the gate dropped from "oop-free" to just
  `new_ops.is_empty() && anewarray_ops.is_empty()` (a surviving `New` still has
  no lowering; array ops bail the builder). `static_call_int_shape` →
  `static_call_shape`: accepts reference args (`L…`/`[…`, passed as the raw
  pointer in one GPR slot) and an int/void/**reference** return; still rejects
  `long`/`float`/`double` (category-2 / XMM). The per-call tuple now carries the
  return-type byte. `descriptor_has_ref_params` removed.
- **`jit/src/ir.rs`** — `set_invoke_info` carries `ret_type`; the `Op::Call`
  result is typed `IrType::Ref` for an `L`/`[` return (so a returned reference
  flows correctly into a following `astore`/field-load/next-call), else
  `IrType::Int`.
- No lowerer change: the existing `Op::Call` marshalling stores each arg (int or
  pointer) as one i64 to the staging region; the conservative scan covers both
  the spilled args and the live references.

**Validation** (the GC-correctness claim is proven *empirically*, per the
project's hard-won lesson that this area needs real GC stress, not theory):
- jit lib 804/804, differential harness 25/25 (adds
  `invokestatic_reference_arg`: a synthetic object passed as a ref arg, the stub
  reads a field off the marshalled pointer).
- **GC-stress probe** `scratch/ircall/IrCallGc.java`: `process(Node a, Node b)`
  holds two references live across two `invokestatic` calls into an
  allocation-heavy callee; `main` passes freshly-made nodes directly so a/b are
  rooted **only** via the IR frame. == HotSpot (`2721637800000`) gate OFF and ON,
  4× deterministic at 64m, **and under `CRATONVM_DBG_GC_STRESS=1`** (a young GC
  on *every* allocation → GC fires constantly mid-call while a/b are live). The
  references survive — the conservative IR-frame scan roots them correctly under
  maximal GC frequency.
- No regression: at every heap size the IR-call path is **GC-behavior-identical
  to single-pass** (both complete ≥64m, both fault <64m — the sub-64m fault is a
  *pre-existing, backend-independent* VM robustness gap: CratonVM faults instead
  of throwing `OutOfMemoryError` at a too-small heap, where HotSpot's GC keeps up
  at 32m; spun off as a separate task). bt14/18 == HotSpot gate-ON; the oop-free
  `IrCall` probe == HotSpot gate-ON.

**To soak / flip on**: same as inc 21 — run `CRATONVM_JIT_IR_CALL=1` on the app
gauntlet + bt checksums, then flip. The widened scope (oops across calls) makes
the gauntlet soak more important before flipping.

**Next refinements**: `invokespecial` of a statically-resolved target (now
unblocked — the receiver oop is handled by the same conservative scan);
long/float/double args + return (category-2 / XMM marshalling — gated on the IR
path handling category-2 values, which `method_uses_category2` currently
excludes); virtual/special/interface dispatch via inline caches (the
bug-24-sensitive area).

## Increment 23 (Gap B — `CRATONVM_JIT_IR_CALL` flipped default-ON) landed

Status: **landed** on `dev`. Closes Gap B's production-validation step: the
`Op::Call` path for `invokestatic` (inc 21 + the oops-across-call widening of
inc 22) is now the **default**, with `CRATONVM_JIT_IR_CALL=0` as the opt-out
safety net (restores single-pass dispatch for `invokestatic`-bearing methods).
This is the inc-19→20 pattern applied to Gap B: the feature landed inert and
soak-gated; this increment is the soak + flip.

**What landed** (`vm/src/runtime/interpreter.rs`): the three `try_compile` call
sites that fed `ir_emit_calls` from `std::env::var_os("CRATONVM_JIT_IR_CALL")
.is_some()` (default-OFF) now read
`std::env::var("CRATONVM_JIT_IR_CALL").map_or(true, |v| v != "0")` (default-ON,
`=0` opt-out) — byte-for-byte the scalar-new flip (inc 20). No other change; the
inc-21/22 machinery (`Op::Call` lowering, the oop-free→oops-across-call gate, the
conservative IR-frame GC scan) is unchanged.

**The soak (the gating prerequisite).** Because the gate is a *runtime* env var,
the post-flip default and the opt-out are the SAME binary toggled by the var, so
one release build validated both. The decisive invariant is **gate-ON ≡ gate-OFF
on every check** — the flip only changes a default, so any ON≠OFF would be the
flip's fault, and there were none:

- **bt10/14/16/18** == HotSpot (`135854 / 3222190 / 14985902 / 68332206`)
  gate-ON (default) **and** gate-OFF (`=0`).
- **IR-call probes** (`scratch/ircall/`, gitignored): `IrCall` ==
  `23762906400000`, `IrCallGc` == `2721637800000` (including under
  `CRATONVM_DBG_GC_STRESS=1` — a young GC on every allocation, the maximal
  oops-across-call stress for inc 22), `IrCallGcCatch` large-heap == `completed
  total=272016378000000` — all == HotSpot, gate-ON and gate-OFF.
- **~20-program bench differential** (gate-ON vs gate-OFF, timing masked,
  cross-checked to HotSpot): `binarytrees`, `fannkuch`, `IntegrationTest`,
  `GenPair`, `FieldCheck`, `NBody3D`, `Benchmark` (all 10 kernel checksums),
  `QuickBench`, `MatrixJIT`, `MatrixScale`, `IntrinsicBench` (checksums),
  `FullStackBench`, `TestLambda`/`TestStream`/`TestSwitch`/`TestEnum`/
  `TestGenerics` — all **ON==OFF==HotSpot**.
- **jit lib 804/804**, **`ir_vs_singlepass` 25/25**, release VM build clean.

**The one non-match is pre-existing and orthogonal.** `NBodyMini` prints a `double`
in plain-decimal where HotSpot uses scientific notation
(`0.000000000018033933843323614` vs `1.8033933843323614E-11`) — *value-identical*,
reproduced by the pre-flip `dev` binary, and unrelated to `invokestatic` dispatch
(it is a `Double.toString` notation gap). Gate-ON == gate-OFF on it, so the flip
did not cause or change it.

**Scope note (why the gauntlet risk is bounded).** The IR_CALL path fires only on
`invokestatic` with int/ref (not category-2) args+return in a method with no
`new`/`anewarray`. `long`/`float`/`double`-bearing hot methods (e.g.
`QuickBenchLong`) bail via `method_uses_category2`, and virtual/special/interface
dispatch never takes this path — so most real-app hot methods bypass it entirely.
The full kafka/spring/tomcat/hibernate suites were **not** re-run here (heavy; the
narrow slice rarely fires); the GC-stress oops-across-call probe is the targeted
proof of the inc-22 risk, and `=0` remains the opt-out if a suite ever regresses.

**Next refinements** (unchanged from inc 22, now the live frontier):
`invokespecial` of a statically-resolved target; category-2 (long/float/double)
args + return; virtual/special/interface dispatch via inline caches.

## Increment 24 (Gap B — `invokespecial` → `Op::Call`, gated) landed

Status: **landed** on `dev`, **default-OFF behind `CRATONVM_JIT_IR_CALL_SPECIAL`**.
Extends the `Op::Call` lever from `invokestatic` (inc 21/22/23) to a resolved
**non-`<init>` `invokespecial`** — a `super.m(…)` / private / otherwise
non-virtual instance call. It lands inert + validated (unit + differential +
live soak); flipping it on is a follow-up (like inc 21→23 for invokestatic),
gated on the broader app-gauntlet soak since it would put a dispatch path live by
default.

**Why it's a small, sound extension.** `invokespecial` is *statically resolved*
(no virtual dispatch), so it reuses the entire `invokestatic` machinery — the
`Op::Call` node, the `ir_lower` `Op::Call` arm, the `invoke_dispatch` ABI, the
leaked `JitInvokeInfo`, and the conservative IR-frame GC scan — with exactly two
deltas: the **receiver is marshalled as arg0** (`num_jit_args = 1 + descriptor
args`) and the `JitInvokeInfo` carries **`invoke_kind = 1`** so `invoke_dispatch`
does the non-virtual dispatch to the resolved target. Both are precisely what the
single-pass backend already does for `invokespecial` (`invoke_kind` 1, receiver
included), so the IR path produces byte-identical dispatch arguments.

**GC-safety** is the inc-22 argument unchanged: the IR lowerer spills every value
to a frame slot (no oop in a register across a call), the GC is non-moving while a
JIT frame is active, and `JitEntryGuard` conservatively scans the frame slots at
every safepoint — so the receiver oop (and any reference args) live across the
call are pinned, never reclaimed or relocated. No oop map needed.

**What landed**
- **`jit/src/ir.rs`** — the `invokespecial` (0xb7) arm gains an `Op::Call` path:
  if the pc is in `invoke_info` (the caller populated it), lower exactly like
  `invokestatic` (pop `num_args`, thread the memory token, push a non-void
  result); otherwise fall through to the existing elidable-`<init>` elision. A pc
  in neither bails to single-pass. (The two are mutually exclusive: a `<init>`
  method has a `new` → not call-eligible.)
- **`jit/src/lib.rs`** — a new `try_compile` parameter `ir_emit_special_calls`.
  The Gap-B gate now admits `invokestatic` (under `ir_emit_calls`) **and**
  non-`<init>` `invokespecial` (under `ir_emit_special_calls`); for a special
  call it sets `num_args = static_call_shape(desc) + 1` (the receiver) and
  `invoke_kind = 1`. `static_call_shape` still rejects category-2 args/returns, so
  only int/reference receivers+args+returns are admitted (the receiver is always a
  reference → one GPR slot).
- **`vm/src/runtime/interpreter.rs`** — the 3 `try_compile` sites pass
  `ir_emit_special_calls` from `CRATONVM_JIT_IR_CALL_SPECIAL` (default-OFF). No
  other VM change (the cache already routes `Op::Call` methods via
  `try_call_with_context`).

**Tests**
- `jit/tests/ir_vs_singlepass.rs::ir_vs_singlepass_invokespecial_instance_call` —
  **executes** the IR-emitted call: `int f(Corpus o, int n){ return o.g(n); }`
  via `invokespecial` to `g(I)I`, with a stub `invoke_dispatch` that reads field 0
  off the marshalled receiver pointer and returns `recv.x + n`. IR == host across
  sign/zero edge cases — proves receiver-first marshalling, `num_jit_args = 2`,
  and `needs_context` at the machine-code level.
- `jit/src/lib.rs::ir_special_call_wiring_routes_through_ir_only_with_flag` —
  crosses the two flags (`ir_emit_calls` off, `ir_emit_special_calls` on, and vice
  versa) to prove `invokespecial` routes through the IR pipeline (`IR_LOWER_
  COMPILES == 1`) **iff** `ir_emit_special_calls` is on — independent of the
  invokestatic gate. Guards against a vacuous validation (single-pass also
  dispatches `invokespecial`).
- jit lib **816/816**, differential harness **26/26**, `cratonvm-vm` builds clean.

**Live soak** (worktree `CratonVM-irspecial`, `CRATONVM_JIT_IR_CALL_SPECIAL`
toggled; the gate is a runtime env var so one build validates both paths):
- `scratch/irspecial/IrSpecial.java` (a hot `f` whose only invokes are
  `super.add`/`super.poly` → real `invokespecial`) == HotSpot (`1442980800000`)
  gate-ON **and** gate-OFF, and `CRATONVM_DBG_IR_CALL` confirms the path fires
  (non-vacuous — `super.*` is the reliable non-`<init>` `invokespecial` source
  since modern javac compiles private-method calls as `invokevirtual`).
- `scratch/irspecial/IrSpecialGc.java` (the receiver `this` + two `Node` refs live
  across two allocation-heavy `super.consume` `invokespecial` calls, rooted only
  via the IR frame) == HotSpot (`2721637800000`) gate-ON and gate-OFF at `-Xmx`
  4g/8g **and** under `CRATONVM_DBG_GC_STRESS=1` at 2g (a young GC on every
  allocation → constant GC mid-`invokespecial` while the refs are live). The
  conservative IR-frame scan roots the receiver + ref args under maximal GC
  frequency. (At `-Xmx 2g` without GC-stress the heavy allocator faults with empty
  output **identically gate-ON and gate-OFF** — the inc-22-documented pre-existing
  small-heap robustness gap, backend-independent, not from this change.)
- No regression with `CRATONVM_JIT_IR_CALL_SPECIAL=1`: bt10/14/16/18 == HotSpot
  (`135854 / 3222190 / 14985902 / 68332206`) and the invokestatic `IrCall` probe
  == HotSpot (`23762906400000`) — the special gate does not perturb the
  invokestatic path or the GC checksums.

**To soak / flip on** (the remaining production step, like inc 21→23): run
`CRATONVM_JIT_IR_CALL_SPECIAL=1` across the kafka/spring/tomcat/hibernate gauntlet
+ bt checksums, then default the flag on. The widened reach (every `super.`/
non-virtual instance call, receiver oop live across the call) makes the gauntlet
soak the gating step before the flip.

**Next refinements**: category-2 (long/float/double) args + return (XMM
marshalling, gated on the IR path handling category-2 values); virtual /
interface dispatch via inline caches (the bug-24-sensitive area).

## Increment 25 (category-2 foundation — `long` arithmetic on the IR path) landed

Status: **landed** on `dev`, **default-OFF behind `CRATONVM_JIT_IR_LONG`**. The
first slice of the category-2 (64-bit value) foundation the "next refinements"
above are gated on. Until now the IR pipeline typed every value as 32-bit `Int`
and `method_uses_category2` bailed the *whole* pipeline on any `long`/`double`
opcode (so even pushing a long needed `lload`, which bailed). This slice admits
**long-arithmetic leaf methods** to the optimizing IR path.

**Key finding — most of the machinery already existed.** The IR builder already
lowers `ladd`/`lsub`/`lmul`/`lneg`/`i2l`/`l2i`/`lconst`/`lload_0..3`/
`lstore_0..3`/`lreturn` to `IrType::Long` nodes, and the lowerer already emits
correct 64-bit code for them (`ADD/SUB/IMUL/NEG` with `REX.W`, `Op::I2L`=MOVSXD,
`Op::L2I`=MOV EAX,EAX, `Op::Const(Long)`=`MOV RAX,imm64`, `Return`=RAX). It was
**dammed** by two things, both fixed here:
1. **The cat-2 gate** bailed before the builder ran.
2. **The two-slot parameter layout was wrong.** `IrBuilder::new` types every
   `Param` `Int` and packs them one-per-slot (`Param(i)` at `locals[i]`). But a
   `long`/`double` occupies **two** JVM local slots, so `(long a, long b)` reads
   `b` via `lload_2` — and `locals[2]` was empty (b had been placed at
   `locals[1]`). The result was a silent miscompile of every multi-long-param
   method.

**What landed**
- **`jit/src/ir.rs`** — `IrBuilder::set_param_types(&[IrType])` re-lays-out the
  parameter locals with the JVM two-slot convention (a `long`/`double` param
  advances the slot cursor by 2) and types each `Param` from the descriptor. The
  `Param` node *index* stays the JIT-arg index (the lowerer reads param `i` from
  prologue slot `(i+1)*8` — one register per parameter, unchanged); only the
  `locals` placement and node type change.
- **`jit/src/lib.rs`** — a new `try_compile` parameter `ir_emit_long`. When on,
  the gate is relaxed to admit a method that uses `long` **iff** it is (a)
  double/float-free (`method_uses_double`) and (b) int-`idiv`/`irem`-free
  (`method_has_int_div`). The latter keeps a `long` value off a deopt point
  (the div guard's resume cannot yet reconstruct a `long` slot — a follow-up, the
  same deferral the FP-slot resume already documents), so a `long` is only ever
  live in a safepoint-free leaf. `ir_param_types(descriptor, is_static)` computes
  the JIT-arg-order types fed to `set_param_types`. A `CRATONVM_DBG_IR_LONG`
  diagnostic prints when a long method actually takes the IR path (the
  non-vacuity proof for the live soak). Unhandled long opcodes (`ldiv`/`lrem`,
  `lshl`/`land`/…, `lcmp`, `ldc2_w`, wide `lload`/`lstore`) still hit the
  builder's catch-all and bail to single-pass — the safe fallback.
- **`vm/src/runtime/interpreter.rs`** — the 3 `try_compile` sites pass
  `ir_emit_long` from `CRATONVM_JIT_IR_LONG` (default-OFF).

**Soundness.** `set_param_types` is only invoked when `ir_emit_long` is on, and
for an all-category-1 signature it reproduces the existing one-slot layout
exactly (inert). The relaxation is double/float- and int-div-free, so the only
new methods admitted are long-arithmetic leaves the builder fully lowers; the
64-bit lowering was already proven by the single-pass long path it mirrors.

**Tests**
- `jit/tests/ir_vs_singlepass.rs` — four executing differentials (full i64):
  `ir_vs_singlepass_long_add` (64-bit wrap + a genuinely-64-bit add a 32-bit op
  would truncate), `…_long_two_slot_params` (`a*b-b` via `lload_2` — the layout
  fix), `…_long_mixed_int_long_param` (`int a` at slot 0, `long b` at slots 1-2),
  `…_long_to_int_return` (`l2i` truncation). IR == single-pass == host.
- `jit/src/lib.rs::ir_long_wiring_routes_through_ir_only_with_flag` — proves a
  long method routes through the IR pipeline (`IR_LOWER_COMPILES == 1`) **iff**
  `ir_emit_long` is on (guards against a vacuous single-pass fall-through).
- jit lib **819/819**, differential **30/30**, `cratonvm-vm` builds clean.

**Live soak** (worktree `CratonVM-irlong`, `CRATONVM_JIT_IR_LONG` toggled):
- `scratch/irlong/IrLong.java` (`mix` — a pure long leaf; `loop` — a long
  accumulator with an int-counter loop, both slots 0-3 / short forms) == HotSpot
  (`3588644437398634000`) gate-ON **and** gate-OFF, and `CRATONVM_DBG_IR_LONG`
  confirms **both** methods take the IR path (2 emit lines — non-vacuous).
- No regression with `CRATONVM_JIT_IR_LONG=1`: bt10/14/16/18 == HotSpot
  (`135854 / 3222190 / 14985902 / 68332206` — bt is long-heavy, so a long
  miscompile would move the checksum), and the invokestatic `IrCall` probe ==
  HotSpot (`23762906400000`, no cross-gate interference).

**Limitations (this is a first slice) / next sub-slices**, in rough order:
wide `lload`/`lstore` (slots ≥ 4) and `ldc2_w` (long constants); `lcmp` +
long-fed branches; `lshl`/`lshr`/`lushr`/`land`/`lor`/`lxor`; `ldiv`/`lrem` (need
the long deopt-resume so a `long` can be live at the div guard); then the
**`double`/`float`** half (XMM registers + FP-slot deopt resume); and finally
**long/double *call* args + returns** (`static_call_shape` category-2 marshalling
— the original "category-2 call args" item, now unblocked at the value level).

## Implementation steps (ordered)

1. **φ/branch lowering repro + fix** (Front 1.1) — unblocks everything.
   ✅ Done: the SIGSEGV fix covered single-return branchy *expressions*, and
   increment 11 fixed the **conditional-early-return (multiple `ireturn` points)**
   miscompile increment 10's harness caught (DCE now roots from all returns).
2. **Differential self-check harness** — ✅ **Done (increment 10)**:
   `jit/tests/ir_vs_singlepass.rs` compiles a corpus through both backends via
   the `optimize` toggle, executes both, and asserts equal results. Already
   caught (and now regression-guards) the multi-return miscompile.
3. **Relax the IR gate** branch-free → branchy → calls → loops, each behind a
   soak flag (Front 1.2). **In progress**: branch-free / branchy / early-return /
   loop int methods all take the IR path and pass the harness; increment 12 added
   `i2b`/`i2c`/`i2s`, increment 13 added `tableswitch`/`lookupswitch`, increment 14
   added int-category `getfield` (the first real `Op::Load`, with a synthetic-heap
   real-execution harness).
   **Remaining**: `putfield`→`Op::Store` (needs scheduler memory ordering), then
   `new` / `invoke*` / array ops — builder emission of those + the rest of the
   real-helper harness. ← next.
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
