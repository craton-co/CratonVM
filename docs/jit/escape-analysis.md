# Escape analysis and scalar replacement: what is proved, what is refused

Scope: the P1 item *"Escape analysis and scalar replacement"* of
`docs/known-issues/deep-research-vm-c2.md` — identity/hash, synchronization,
exceptions, arrays, partial escape, deopt reconstruction.

Subject: `jit/src/escape_analysis.rs`, the **sea-of-nodes** escape analysis
reached from the tier-2 IR pipeline. It is not the bytecode-level
`analyze_escapes` in `jit/src/x64.rs`, which is a different function with the
same name driving the single-pass backend. Fixing one does not fix the other.

---

## 1. Three questions, not one

Scalar replacement deletes a heap object. Three independent facts must hold:

| # | Question | Machinery | Failure if skipped |
| --- | --- | --- | --- |
| 1 | **Reachability** — can anything outside this frame reach it? | `EscapeState` + `propagate_escape_states` | the object is mutated/read through a reference we deleted |
| 2 | **Value** — can every surviving field read be answered *with the value the field held at that point*? | `field_stores` (positional) + `LoadResolution` | a load is folded to a value from its own future |
| 3 | **Identity** — does anything observe the object's *address*? | `find_identity_observations` | `==`, identity hash or `monitorenter` on a deleted address |

Before this change only question 1 was asked. Question 2 was answered with a
last-write-wins field map, which is how `Foo o = new Foo(); int a = o.x; o.x =
42;` folded `a` to `42`. Question 3 was not asked at all: `MonitorEnter` /
`MonitorExit` uses were accepted unconditionally.

---

## 2. The lattice

```text
  NoEscape  <  PartialEscape  <  ArgEscape  <  GlobalEscape
  ▲                                                       ▲
  bottom: optimisable                        top: fail-closed answer
```

`join` is `max`. `propagate_escape_states` only ever joins upward, so every
intermediate value under-estimates escape and the fixed point over-approximates
it. Non-convergence escalates every node to `GlobalEscape`
(`escalate_all_to_global`), which disables every consumer.

### `PartialEscape` is a *report*, never a propagated state

`PartialEscape` is a path-sensitive refinement applied **after** the fixed
point, in `analyze_escapes`. It lowers an allocation's reported state from
`ArgEscape`/`GlobalEscape` when every site at which the object escapes is in
`Graph::cold_nodes`. Lowering a lattice state is unsound in general, so it is
fenced three ways:

* it is never written into the `ConnectionGraph`, so it cannot feed back into
  the join and weaken another node;
* it appears only in `EscapeAnalysisResult::escape_states`,
  `EscapeAnalysisStats` and `partial_escapes` — all informational;
* every *acting* consumer (`find_scalar_replacements`, `find_lock_elisions`)
  tests `== NoEscape` against the connection graph, so a `PartialEscape` object
  is treated exactly like the `ArgEscape`/`GlobalEscape` object it really is.

It orders below `ArgEscape` because that is the direction a future
partial-escape applier moves it in. The predicate for "escapes" is therefore
`EscapeState::may_escape()` (`!= NoEscape`), **not** `>= ArgEscape`.

`Graph::cold_nodes` is empty by default and has no producer, so today no
allocation is ever classified partial and behaviour is byte-identical to before.
`PartialEscapeInfo::escape_sites` names the nodes at which a future applier
would have to materialize.

### What each state means

| State | Cause | Consumers |
| --- | --- | --- |
| `NoEscape` | no publication anywhere | scalar replacement, lock elision |
| `PartialEscape` | every escape site is cold | none (reported only) |
| `ArgEscape` | passed to a callee | `stack_allocatable` (computed, never read — stack allocation is not implemented) |
| `GlobalEscape` | returned; stored into an escaping or **unnameable** holder; any unconverged run | none |

### Bug found and fixed here: writes to an unnameable holder did not escape

The store rule read the holder's state with `ConnectionGraph::get_escape`, which
reports an *unseen* node as `NoEscape`. A holder that resolves to no allocation
— a `putstatic` base, an opaque reference, a bridge gap that mapped a holder to
`usize::MAX` — therefore had `holder_escape == NoEscape` and the rule never
fired. **An object published into a static field stayed `NoEscape`**: scalar-
replaceable and lock-elidable, while reachable from every thread in the VM.

`Op::Param` holders were covered (the build phase marks them `GlobalEscape`, for
the lazy-init-getter shape); nothing covered the rest. The rule now joins
`GlobalEscape` into `holder_escape` whenever the holder's points-to set is
empty. Test: `object_stored_into_an_unnameable_holder_is_global_escape`.

---

## 3. Positional store tracking, and why load forwarding is now *correct*

### The defect

`ScalarReplacementInfo::field_values[f]` records the value of the **last** store
to field `f` in program order. `apply_ea_to_ir` forwarded *every* replaced load
of `f` to it. For

```java
Foo o = new Foo();
int a = o.x;     // reads the zero default
o.x = 42;
```

that folds `a` to `42`. The applier now refuses the object whenever a store to
the loaded field has a higher node id than the load — correct, but it refuses
the optimization instead of performing it, and it does nothing for the
*branchy* variant below, where no store has a higher id than the load:

```java
Foo o = new Foo();
if (c) o.x = 1; else o.x = 2;
int a = o.x;     // applier forwards `field_values[0]` == 2 — MISCOMPILE on the `c` path
```

### The fix

`ScalarReplacementInfo` now carries `field_stores: Vec<Vec<FieldStore>>` — per
field, **every** store with its node id, sorted ascending = program order — and
`load_values: Vec<(NodeId, LoadResolution)>`, the per-load answer.
`LoadResolution` is deliberately three-valued:

| Variant | Meaning |
| --- | --- |
| `Value(n)` | the load reads node `n` |
| `ZeroDefault` | no store dominates the load; it reads the allocation's zero value |
| `Unknown` | not provable — the caller must keep the load |

Collapsing `ZeroDefault` and `Unknown` into one `Option<NodeId>` is exactly how
a load-before-store gets a value from its future.

**A candidate that reaches `EscapeAnalysisResult` never contains `Unknown`** —
`find_scalar_replacements` refuses the whole object instead, because leaving an
unanswerable load in the graph while eliding the allocation makes that load read
a deleted object.

### The dominance stand-in and its gate

"Which store does this load see?" is a dominance question, and **the EA graph
cannot ask it**: `escape_analysis_from_ir` rewrites the production
`Op::Load`/`Op::Store` into the compact `[holder, value]` / `[holder]` layout
and drops input 0 (control) and input 1 (memory). There is no CFG here.

So program order — ascending `NodeId`, which is creation order, which the bridge
derives from `ir::Graph` order, which the builder derives from bytecode order —
is used as a stand-in, gated on `program_order_proves_dominance(graph)`:

* any `Op::If` ⇒ false. This is the only way control can diverge, and therefore
  also the only way a loop can exist (a loop with no exit branch does not
  terminate) — which is what closes the back-edge case where a *higher*-id store
  in a loop body runs before a *lower*-id load on the next iteration. It also
  closes a LICM hazard: a hoisted load keeps its node id but changes its
  position, and there is no loop to hoist out of in a branch-free graph.
* any `Op::Merge` ⇒ false. Checked separately from `If` because the bridge maps
  `ir::Op::Region` (the loop header) to `Op::Other`, so `Merge` is the only join
  op that survives translation.
* any `Op::Phi` with more than one input ⇒ false. A multi-input φ can only exist
  at a join; a single-input φ is a degenerate copy (the transparent-alias shape
  `find_scalar_replacements` already accepts) and implies no divergence.

**Exceptions are deliberately not on the list.** A `catch`/`finally` handler body
is never built into the IR: `ir::IrBuilder::build` skips handler bytecode
outright (the "STUB-S8" skip), because a JIT frame never takes an exception edge
— an exception makes the compiled body return the `i64::MIN` sentinel and the
runtime re-runs or precisely resumes the method in the interpreter. So a throw
inside a compiled body *leaves the frame*; it cannot branch to a lower-id load.
**If handler bodies are ever compiled, this predicate must gain a `may_throw`
term.**

### What survives a `false` answer

`dominance_proved == false` is not a bug, it is the fail-closed answer:

* a field with **no store anywhere** still resolves, to `ZeroDefault` — no
  control flow can change what a never-written field holds;
* an object with stores but **no loads** is still fully replaceable — nothing
  has to be answered;
* everything else is refused.

### Is load forwarding now correct?

**Yes, for the analysis.** `load_values` answers each load with the store that
actually precedes it, `ZeroDefault` for a load before every store, and refuses
rather than guesses when program order is not dominance. The in-module applier
`apply_scalar_replacement` was rewritten to consume `load_values` (it had the
same last-write-wins bug) and to be all-or-nothing.

**Not yet end-to-end.** The production applier `apply_ea_to_ir` still reads
`field_values` and still refuses via its own id-comparison heuristic — see §6
for the edit that switches it over. Until then the pipeline is *correct but
pessimistic*: EA can answer the load, the applier declines to use the answer.

---

## 4. Identity sensitivity

Three JVM operations observe an object's address without publishing it:

* `if_acmpeq` / `if_acmpne` — two scalar-replaced objects with equal fields are
  indistinguishable; two heap objects are not;
* `System.identityHashCode` / a non-overridden `Object.hashCode` — a stable
  per-object value derived from the header;
* `monitorenter` / `monitorexit` — the monitor *is* the header.

`find_identity_observations` reports `(allocation, observing node)` for every
**live** such node, resolving through φ aliases and field loads. Any allocation
it names is excluded from `scalar_replaceable`, whatever its escape state.
`EscapeAnalysisStats::identity_blocked` counts the `NoEscape` objects refused
for this reason alone — the measurement that says what an identity-folder would
be worth.

### "unless the observation is itself eliminated" is taken literally

The observing node must already be `Op::Dead`. Anything weaker requires this
module and its consumer to agree about a *future* elimination, and they do not:
this module *offers* lock elisions, and `apply_ea_to_ir` independently **refuses**
one whose monitor a safepoint slot names, whose memory chain it cannot splice,
or whose value is still read. Each refusal would leave a live `monitorenter` on
an object this pass just deleted.

So the supported way to scalar-replace a synchronized object is **two-phase**:

1. `analyze_escapes` → `elide_locks`;
2. `apply_lock_elision` (the monitors become `Op::Dead`);
3. `analyze_escapes` **again** on the mutated graph — now nothing observes the
   identity, and the object replaces.

Test: `synchronized_object_is_not_replaced_until_the_monitor_is_eliminated`
covers both halves.

### Producer status

`Op::RefCompare` and `Op::IdentityHash` are new variants with **no producer**.
`ir_op_to_ea_op` maps `ir::Op::Cmp(_)` to `Op::Other` and an identity-hash call
to `Op::Call`. The *outcome* is already fail-closed — `Op::Other` hits the
catch-all and `Op::Call` escapes the object — but it is incidental,
unmeasurable, and would silently become wrong if `Op::Other` were ever relaxed
or if `Op::Call` gained an "inlined intrinsic, does not escape" arm. §6 has the
wiring edit.

---

## 5. Arrays and exceptions

**Arrays are out of scope by construction, not by omission.**
`find_scalar_replacements` only accepts `Op::New`; `field_in_range` returns
`false` for every `Op::NewArray`, so an array never resolves a field edge; and
`ir_op_to_ea_op` maps `ir::Op::ArrayLoad`/`ArrayStore` to `Op::Call`, which
escapes the array reference to `ArgEscape`. That is correct today because the IR
lowerer has no scalar-array path at all and a live `Op::NewArray` makes the
optimizing tier decline the method outright. Scalar-replacing a
constant-length array needs a lowerer change first, not an analysis change.

**Exceptions** are covered in §3: handler bodies are not compiled, so there is no
intra-method exception control flow for the analysis to model. This is a
*dependency*, not a proof — it is recorded here so that whoever enables handler
compilation knows to revisit `program_order_proves_dominance`.

---

## 6. Edits required outside `jit/src/escape_analysis.rs`

All in `jit/src/lib.rs`, which is another agent's file. Cited by symbol, not
line, because that file is being edited concurrently.

### 6.1 Use the per-load answer (priority 1 — turns a refusal into an optimization)

`plan_scalar_replacement`, the `LAST-WRITE-WINS HAZARD` block and the
`load_plans` loop below it. Today:

```rust
// refuse the object if any store to the loaded field is later
for &(l, lf) in &loads {
    if stores.iter().any(|&(s, sf)| sf == lf && s > l) {
        return None;
    }
}
...
let value = match info.field_values.get(f) { ... };
```

Replace both with the analysis's answer:

```rust
for &(l, _f) in &loads {
    let value = match info.load_value(ea_load_for(l)) {
        escape_analysis::LoadResolution::Value(ea_val) => match reverse_map.get(&ea_val) {
            Some(&v) if ir_graph.node_opt(v).is_some_and(|n| n.op != ir::Op::Dead) => Some(v),
            _ => return None,
        },
        escape_analysis::LoadResolution::ZeroDefault => None, // caller materialises Const(0)
        escape_analysis::LoadResolution::Unknown => return None,
    };
    load_plans.push((l, value));
}
```

`ea_load_for(l)` is the forward `id_map` lookup (IR id → EA id); the loop
already has the reverse map, so either pass `id_map` in or build the forward map
alongside `reverse_map`. Deleting the `s > l` refusal is only safe *after* this
switch — it is what currently prevents the `field_values` miscompilation.

The `field_index(l)` helper stays: `virtual_object_info_for` still needs it.

### 6.2 Emit the identity ops (priority 2 — makes a fail-closed outcome intentional)

`ir_op_to_ea_op`:

* `ir::Op::Cmp(_)` where **both** compared inputs have `ir::IrType::Ref` ⇒
  `EaOp::RefCompare`. (`ir_op_to_ea_op` takes only the `Op`, so this needs the
  graph — do it in the `match &ir_node.op` in `escape_analysis_from_ir`, next to
  the existing `Load`/`Store` field-index recovery, and leave `ir_op_to_ea_op`
  as the fallback.)
* an `ir::Op::Call` to `System.identityHashCode` or a non-overridden
  `Object.hashCode` ⇒ `EaOp::IdentityHash`. This is a *relaxation* (it stops
  escaping the receiver), so it must only fire on a resolved, known-intrinsic
  callee.
* `ir::Op::MonitorEnter`/`MonitorExit` if and when the builder emits them —
  today `elide_locks` is empty for every IR-derived graph because no arm can
  produce them.

### 6.3 Feed cold-path information (priority 3 — enables partial escape at all)

`escape_analysis_from_ir` gains a `&profile::MethodProfile` (or the
`ir_branch_hints` map already computed in `try_compile_inner`) and calls
`ea.mark_cold(id)` for every node on a never-taken branch. Until then
`partial_escapes` is always empty. Nothing regresses if this is never done.

### 6.4 Honour `dominance_proved` in the deopt recipe (priority 3)

`virtual_object_info_for` builds `ir_lower::VirtualObjectInfo::field_values`
from `info.field_values` (last write wins) plus `store_ctrls` for
`ir_lower::frame_value_for_object`'s dominance check. `info.field_stores` now
gives it the per-store positions, so the "unproven dominance" bail can become
"pick the latest store that dominates *this safepoint*" — see §7.

---

## 7. Re-enabling elision for snapshot-named allocations

### Why it is off

`plan_scalar_replacement` sets `elide_alloc = false` when a safepoint snapshot
names the `Op::New` and no virtual-object descriptor will exist:

```rust
if ea_snapshot_names(ir_graph, new_node)
    && !(deopt_descriptor_available && virtual_object_info_for(...).is_some())
{
    elide_alloc = false;
}
```

`deopt_descriptor_available` is `scalar_deopt_enabled() && deopt_real_enabled()`
— both env-gated and off by default. `IrBuilder` snapshots at **every bci**, so
every `Op::New` is snapshot-named. Net effect: **allocation elision does not
fire on builder-produced IR at all.** Loads are still forwarded and stores still
die; only the `New` survives.

That gate is correct as written. A `SafepointSnapshot` slot is a bare `NodeId`;
it cannot spell "eliminated". Killing the `New` leaves the slot naming an
`Op::Dead` node, which `ir_optimize::eliminate_dead_nodes` normalises to
`NO_NODE`, which `ir_lower::frame_value_for` maps to `FrameValue::Undefined`,
which every resume sink turns into `Value::Int(0)` — **a null where a live object
was**, silently.

### What it would take, in order

The prize is not a change in `escape_analysis.rs`; it is making the *refusal*
representable so that eliding is safe even when the recipe is imperfect.

1. **Make `Undefined` unreachable for a killed producer.** Per
   `docs/jit/deopt-metadata.md` §2, `ir_lower::frame_value_for_object`'s six
   `return FrameValue::Undefined` bails become
   `FrameValue::MaterializationRequired(EliminatedValue::allocation(new_id,
   class_id, cause))`, and `frame_value_for`'s "no machine location" fallback
   splits: `Op::Dead` node ⇒ `MaterializationRequired(… Unclassified)`,
   `NO_NODE` slot ⇒ keep `Undefined`. After this, a slot naming an eliminated
   allocation with no recipe *refuses to resume* (whole-method re-run, always
   valid) instead of reconstructing null.
2. **Have `apply_ea_to_ir` mark, not strand, the slot.** `docs/jit/deopt-metadata.md`
   §5 item 1: after the rewiring loop, walk `graph.safepoints` and for each slot
   naming a killed node either redirect it (forwarded load) or leave it naming
   the `Op::New` *and record the object in the `ScalarReplacementMap`*. The
   forwarding half is already done; the `New` half depends on step 1 to be safe.
3. **Then, and only then, relax the gate.** `elide_alloc` may become
   unconditional w.r.t. snapshots: worst case the frame refuses and the method
   re-runs; best case it materializes. That is the trade the review asks for —
   a *refusable* elimination rather than a silent null.
4. **Sharpen the recipe with `field_stores`.** With step 1 in place, an
   imperfect recipe is safe, but a *precise* one is better: pick, per safepoint,
   the latest store to each field whose control block dominates that safepoint's
   block. `ScalarReplacementInfo::field_stores` provides exactly the
   `(store node, value)` list that needs; `VirtualObjectInfo::store_ctrls` maps
   each store to its block.
5. **Nested virtuals and inlined scopes remain out.** `frame_value_for_object`
   bails on a nested virtual object, and `FrameState::caller` is hard-coded
   `None` in both backends, so an eliminated object inside an inlined callee
   cannot be described at all. Neither is a blocker for step 3 (both bail to
   `MaterializationRequired` once step 1 lands), but both cap how often the
   recipe succeeds.

**Nothing in steps 1–4 is in `escape_analysis.rs`.** What this module now
contributes is (a) the positional store record step 4 needs, and (b) a guarantee
that the objects it offers have *no* unanswerable load and *no* identity
observation — so that when the gate is relaxed, the only remaining risk is the
deopt recipe, not the value flow.

---

## 8. To reconcile

1. **`apply_ea_to_ir` and this module disagree about which load values to use**
   (§6.1). Until that edit lands, EA computes a correct per-load answer that the
   applier ignores in favour of `field_values` + a refusal heuristic. Both are
   *safe*; only one is precise.
2. **`ScalarReplacementInfo::field_values` now has exactly one honest consumer**:
   the deopt recipe in `virtual_object_info_for`, where "the value at the end of
   the object's live range" is a defensible reading. It must not be used for load
   forwarding. The doc comment says so; a type-level split would say it better.
3. **The branchy-graph regression is a correctness fix, not a loss.** Objects in
   methods with any branch, join or multi-input φ are no longer offered for
   scalar replacement when any of their loaded fields has a store. They were
   previously offered *and mis-forwarded*. Recovering them needs real control
   edges in the EA graph (see §9), not a relaxation of the gate.
4. **`stack_allocatable` is still computed and still never read.** Stack
   allocation is not implemented. `PartialEscape` allocations are deliberately
   excluded from it.
5. **`is_partial_escape` / `find_materialization_points` are the old structural
   heuristic** and are now documented as distinct from `EscapeState::PartialEscape`.
   They answer "some use escapes and some does not", which is true for
   `new Foo(); o.x = 1; f(o);` even though that object escapes on every
   execution. Only the cold-path classification is a basis for sinking. They
   should be folded into `PartialEscapeInfo` when a partial-escape applier exists.
6. **The identity-observation refusal is stricter than C2's.** A freshly
   allocated object that never escapes can never be `==` to any reference the
   method did not derive from it, so `new Foo() == someParam` is statically
   `false` and both the comparison and the object could go. We refuse instead,
   because folding the comparison is the applier's job and the applier has no
   such support. `stats.identity_blocked` measures what that costs.

---

## 9. The one structural change that would unlock the rest

Give the EA graph its control edges. `escape_analysis_from_ir` currently
rewrites memory ops to `[holder, value]` / `[holder]`, discarding control and
memory. If it instead kept the control input — e.g. a third operand, or a
parallel `Vec<Option<NodeId>>` of control nodes keyed by EA node id — then:

* `program_order_proves_dominance` could be replaced by a real dominator query,
  and branchy methods would resolve their loads instead of being refused;
* `LoadResolution` would answer φ-merged fields with a synthesised φ over the
  reaching stores rather than `Unknown`;
* the deopt recipe (§7 step 4) could be built per safepoint without a separate
  `store_ctrls` side channel.

That is a bridge change (`jit/src/lib.rs`) plus a layout-contract change here.
It is the single highest-value follow-up; everything in §3 is scaffolding around
its absence.
