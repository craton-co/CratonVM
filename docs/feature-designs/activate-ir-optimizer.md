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
alias oracle (hoist past a non-aliasing in-loop store) remains future work.

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
