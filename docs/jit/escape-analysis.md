# Escape analysis and scalar replacement: what is proved, what is refused

Scope: the P1 item *"Escape analysis and scalar replacement"* of
the C2 review — identity/hash, synchronization,
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

`Graph::cold_nodes` is empty by default. *As written:* it had no producer.
**Since then** `try_compile_inner` feeds it: `ea_mark_cold_from_branch_hints`
(`jit/src/lib.rs`) calls `ea.mark_cold` for the nodes `ea_cold_control_nodes`
derives from the IR branch hints. The classification stays **report-only**: no
acting consumer reads `PartialEscape`, so this changes `partial_escapes` and the
stats, not the generated code. `PartialEscapeInfo::escape_sites` names the nodes
at which a future applier would have to materialize.

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

**The per-allocation escape (added after this section was written).**
`find_scalar_replacements` computes the graph-wide answer once and then, for
each candidate, uses
`dominance_proved || one_block_proves_dominance(graph, id, &transparent,
&field_stores, &replaced_loads)`. The second disjunct holds when the allocation
and **every** store and load the candidate folds are in one basic block
(`Graph::blocks`, filled by the bridge), and the reference has no φ alias
(`transparent` is the allocation alone). Within a straight-line block ascending
`NodeId` is execution order. Because the allocation is in the same block, each
execution of the block makes a fresh object, so a store from an earlier loop
iteration wrote a different object. This is what lets an object allocated,
filled and read inside a loop body be replaced, even though the loop's own
back-edge `Merge` makes `program_order_proves_dominance` false. A node with no
recorded block, or accesses in different blocks, answers `false`.
`Graph::blocks` is empty by default, and a `Guard` is not a block boundary.

**Exceptions are deliberately not on the list.** A `catch`/`finally` handler body
is never built into the IR: `ir::IrBuilder::build` skips handler bytecode
outright (the "STUB-S8" skip), because a JIT frame never takes an exception edge
— an exception makes the compiled body return the `i64::MIN` sentinel and the
runtime re-runs or precisely resumes the method in the interpreter. So a throw
inside a compiled body *leaves the frame*; it cannot branch to a lower-id load.
**If handler bodies are ever compiled, this predicate must gain a `may_throw`
term.**

### The memory-chain cross-check 

Both stand-ins read "store id < load id" as "the store ran first". That is
true of nodes the builder created in walk order and false for a node a later
pass *created*. The one that matters is `ir_optimize::unroll`: a full unroll
clones the loop's `Op::Load`s with fresh, **high** ids, pins every clone to the
preheader, and rewires the loop's exit edge to that same preheader. A store
that runs after the old loop then has a *lower* id than the cloned loads that
run before it, in the same block — and in a graph the unroll left branch-free,
under `program_order_proves_dominance` as well. Nothing in the id order can
see it.

The memory chain can. The builder threads every memory operation onto one
token chain (a `getfield` included: `self.mem = load`), and a clone keeps its
original token. So the bridge records each EA load's token
(`Graph::load_mem_tokens`), and `resolve_field_load` asks the question twice:
the latest store *by id* before the load, and the latest store *at or before
the load's token*. If they differ the answer is `Unknown` and the object is
refused (`LoadNotAnswerable`). The check can only refuse — it never forwards a
value the id order would not have — and on a builder-order graph the two
always agree. A load with no recorded token (hand-built graphs, compact
layouts) is answered by id order alone, as before. Tests:
`r9_ea_tests::a_load_the_memory_chain_orders_before_a_store_is_not_forwarded_that_store`
and its two agreeing twins.

### Memory tokens are not operands 

`escape_analysis_from_ir` forwards most nodes' inputs verbatim, and that used
to include the incoming memory token (slot 1 of every `[ctrl, mem, …]` node).
Because a `getfield` is on the chain, the token of a call, `ldc`, `getstatic`,
`checkcast`, `instanceof`, `athrow` or non-constant array access is often a
field `Op::Load` — which `is_ref_producer` accepts. The `Call`/`Other`/`Throw`
escape rules therefore escaped the load, and through its field edge every
allocation it could read: `Box b = pair.box; foo();` escaped `b`'s allocation
purely because of instruction order. The default re-packing arm now drops the
slot `ir::memory_token_slot` names (arity-guarded). Nothing can reach an
object through an ordering token, so this removes no real edge. Tests:
`r9_ea_bridge_tests` in `jit/src/ea_ir_bridge.rs`.

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

**Yes, end-to-end too.** §6.1's edit landed: the production
applier's planner `plan_scalar_replacement` consumes `info.load_value` per load
and refuses on `Unknown` (the `LoadResolution::Unknown => return None` arm in
`plan_scalar_replacement`, `jit/src/lib.rs`), and the id-comparison
heuristic is gone. The end-to-end witness is
`ea_load_before_a_later_store_forwards_the_pre_store_value`
(`jit/src/lib.rs`), which asserts the pre-store value is forwarded *and*
pins the precondition that `field_values[0]` — the source the applier used to
read — still holds the wrong (later) value, so the test cannot pass by
accident.

---

## 4. Identity sensitivity

Three JVM operations observe an object's address without publishing it:

* `if_acmpeq` / `if_acmpne` — two scalar-replaced objects with equal fields are
  indistinguishable; two heap objects are not;
* `System.identityHashCode` / a non-overridden `Object.hashCode` — a stable
  per-object value derived from the header (only the first is *recognised* as
  one; see the producer status below);
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

> **Since, verified : both variants have a producer.** The first pass
> of `escape_analysis_from_ir` (`jit/src/lib.rs`) maps an `ir::Op::Cmp(_)` to
> `escape_analysis::Op::RefCompare` when `ir_cmp_is_ref_compare` holds. That is
> a reference compare, and only the null literal (`Op::Const(0)`) is excluded,
> not `Const` in general. It maps an `ir::Op::Call` to
> `escape_analysis::Op::IdentityHash` when `ir_call_is_identity_hash` holds:
> `java/lang/System.identityHashCode(Ljava/lang/Object;)I` as a static call
> (`invoke_kind == 3`). **Since r9 (opt-in) that is the only shape.** The
> bridge used to accept `java/lang/Object.hashCode()I` as an `invokespecial`
> (`super.hashCode()`) too, on the theory that a non-virtual call lands on
> `Object`'s body. Under `ACC_SUPER` it does not: an `invokespecial` naming a
> superclass of the current class selects from the current class's DIRECT
> superclass (JVMS §6.5), i.e. the nearest override, which is arbitrary Java
> that may publish the receiver — and `IdentityHash` escapes nothing, so lock
> elision could act on it. javac only names `Object` there when it IS the
> direct superclass, but a spliced body need not come from javac. Both that
> shape and a *virtual* `hashCode()` now stay `Op::Call`.
> The observed reference is re-packed from IR input 2 to EA input 0.
> `ir_op_to_ea_op`, the graph-free fallback, still maps `Cmp` to `Other` and
> calls to `Call`. The paragraph below is the pre-wiring state.

`Op::RefCompare` and `Op::IdentityHash` were new variants with **no producer**.
`ir_op_to_ea_op` maps `ir::Op::Cmp(_)` to `Op::Other` and an identity-hash call
to `Op::Call`. The *outcome* is already fail-closed — `Op::Other` hits the
catch-all and `Op::Call` escapes the object — but it is incidental,
unmeasurable, and would silently become wrong if `Op::Other` were ever relaxed
or if `Op::Call` gained an "inlined intrinsic, does not escape" arm. §6 has the
wiring edit.

---

## 5. Arrays, aliases, and the production planner

> **Rewritten  against `jit/src/escape_analysis.rs` and
> `jit/src/lib.rs`.** The earlier text of this section said arrays were out of
> scope. That stopped being true when small constant-length arrays became
> scalar-replaceable.

### 5.1 Small constant-length primitive arrays are replaceable

`find_scalar_replacements` accepts an `Op::NewArray` as well as an `Op::New`,
when all of the following hold:

* The length is a compile-time constant. The bridge
  (`escape_analysis_from_ir`) fills `escape_analysis::Op::NewArray { length }`
  from `ir_new_array_const_length`; otherwise the refusal is
  `ArrayRefusal::LengthNotConstant`.
* The length is at most `MAX_SCALAR_ARRAY_LEN` (8); otherwise
  `ArrayRefusal::LengthTooLarge(n)`.
* The elements are primitive. A reference-element array (`anewarray`) is
  refused with `ArrayRefusal::ReferenceElements`.
* No `arraylength` use of the array survives. That refusal is
  `ArrayRefusal::LengthReadNotFolded`.

The element slots are the "fields". `field_in_range` answers
`length.is_some_and(|n| field < n)` for a `NewArray`. The bridge maps an
`ir::Op::ArrayLoad` / `ArrayStore` whose index is a constant
(`ir_array_const_index`) to `escape_analysis::Op::Load(idx)` / `Store(idx)`,
re-packed into the same `[holder]` / `[holder, value]` layout as a field access.
A non-constant index still maps to `Op::Call` (the graph-free fallback
`ir_op_to_ea_op`), which escapes the array.

### 5.2 The narrow-array forwarding rule

A field store keeps its value whole. A narrow array element does not: `bastore`,
`sastore` and `castore` truncate, and the matching load re-extends. Forwarding
the stored value straight to the load is therefore only correct when it already
fits. `plan_scalar_replacement` checks every forwarded array value with
`narrow_array_value_fits(graph, value, load_kind, array_element_type)` and
refuses the **whole object** when it does not fit. The rule:

| Element kind | A forwarded value fits when it is |
| --- | --- |
| `boolean[]` (newarray atype 4, which stores `value & 1`) | `Op::Const(0)` or `Op::Const(1)`, or an `Op::Cmp(_)` result |
| `byte[]` | an `Op::Const` in `i8` range, an `ArrayLoad(MemKind::Byte)`, or `(x << 24) >> 24` (`Shr` of `Shl` by the constant 24) |
| `short[]` | an `Op::Const` in `i16` range, an `ArrayLoad(MemKind::Short)`, or `(x << 16) >> 16` |
| `char[]` | an `Op::Const` in `0..=u16::MAX`, an `ArrayLoad(MemKind::Char)`, or `x & 0xFFFF` (constant on either side) |
| `int`, `long`, `float`, `double` | anything |

The shift and mask shapes are the builder's own `i2b` / `i2s` / `i2c` lowering.
Any other value refuses. The commit that added the rule gives the example
`sipush 300; bastore; baload`, which forwarded 300 where the JVM reads 44. Tests:
`narrow_array_forwarding_tests` in `jit/src/lib.rs`.

### 5.3 The φ alias rule

A φ that uses the allocation is accepted as a *transparent alias* (its uses are
followed as if they were the allocation's) only when all three hold. This is the
`Op::Phi` arm of `find_scalar_replacements`:

1. the φ itself is `NoEscape`;
2. its points-to set is exactly `{alloc}`;
3. **every** input except a control anchor (`Start`, `Merge`, `If`) has a
   points-to set of exactly `{alloc}`.

Anything else refuses the object with `ScalarRefusal::PhiNotTransparentCopy`.
Condition 2 alone is not enough. An input of unknown provenance (a `Param`, a
`Call` result, a field `Load`) contributes nothing to a points-to set, so
`φ(alloc, param)` still resolves to `{alloc}` while carrying the parameter on
one path.

**`Const` is not treated as producing no reference.** Condition 3 used to skip
any input that `is_ref_producer` did not list. But the EA graph is untyped, and
the bridge maps the `aconst_null` literal to `Op::Const(0)` and a `checkcast` to
`Op::Other`. `p = c ? new Foo() : null; p.x` then folded to `0` instead of
throwing an NPE. Now the only skipped inputs are control anchors, so a
`Const(0)` input fails the singleton test and refuses. The lock side makes the
same decision in `produces_no_reference`, which deliberately omits `Op::Const`:
`synchronized (c ? new Object() : null)` had its lock elided and lost the NPE on
the null path. The identity-compare bridge `ir_cmp_is_ref_compare` excludes only
the null literal (`Op::Const(0)` on a `Ref` compare), not `Op::Const` in general.

A multi-input φ whose inputs all resolve to the allocation passes this arm.
Such a graph fails `program_order_proves_dominance`, so any loaded field with a
store resolves to `Unknown` and the object is refused anyway.
`one_block_proves_dominance` (§3) cannot rescue it, because it requires
`transparent` to be the allocation alone.

### 5.4 The production planner: `plan_scalar_replacement`

`apply_ea_to_ir_pinned` (`jit/src/lib.rs`) runs `plan_scalar_replacement` per
candidate. The planner is pure and returns `None` to refuse the whole object:

* the node kind must match: `Op::New` for an object, `Op::NewArray` for an
  array (`info.array_element_type.is_some()`), and loads and stores must be
  `Load` / `Store` or `ArrayLoad` / `ArrayStore` to match. The slot index must be
  recoverable (`ir_array_const_index` for arrays);
* each load takes `info.load_value(ea_load)`. `Value(v)` needs a live IR
  counterpart, and an array value must also pass `narrow_array_value_fits`.
  `ZeroDefault` gets one shared zero constant per `IrType` (`Const(0)` for
  `Int`/`Long`, `ConstF(0)` for `Float`/`Double`; any other type drops the plan).
  `Unknown` refuses. Every load must also pass `ea_splice_feasible`;
* `elide_alloc` starts `true` and is cleared when an earlier EA round's
  descriptor pins the allocation, when a snapshot names a store, or when a
  **consultable** snapshot names the allocation (`ea_consumable_snapshot_names`,
  see §7). The descriptor-pin and consultable-snapshot cases are forgiven when
  `deopt_descriptor_available && virtual_object_info_for(...)` yields a
  descriptor. It is also cleared when a store or the allocation fails
  `ea_splice_feasible`, or when another live node still reads the allocation or
  a store. `deopt_descriptor_available` is `scalar_deopt_descriptor_available()`,
  which is `deopt_real_enabled()` (default on); `CRATONVM_SCALAR_DEOPT` is
  superseded. With no loads and `!elide_alloc`, there is nothing to do and the
  plan is `None`.

`try_compile_inner` runs escape analysis up to `MAX_EA_ROUNDS` (3) times,
because one replacement can expose another (a wrapper holding an array). When
any `Op::New` or `Op::NewArray` survives lowering, the artifact is marked
`has_dispatch` so its failure path is handled.

### 5.5 The loop-hoist refusal

Hoisting interacts with value forwarding, and the refusal lives in LICM
(`jit/src/ir_optimize.rs`), not in this module:

* `loop_has_hard_barrier` is true when the loop body contains any node that is
  neither pure nor control, other than `Load`, `Store`, `Phi` or `Dead`. That
  covers calls, allocations, guards, **monitors**, and `ArrayLoad` /
  **`ArrayStore`** / `ArrayLength`. Such a loop gets **no** load hoisting at
  all.
* An in-loop **field** `Store` is not a hard barrier. `loop_store_clobber` /
  `load_safe_past_clobber` block exactly the loads it could alias, and a store
  through an opaque base blocks every load.
* The restricted read-hoist (on unless `CRATONVM_JIT_NO_LICM_READ_HOIST=1`)
  handles a loop that only reads and guards disqualify. It requires
  `loop_has_hard_barrier && !loop_writes_memory`. `loop_writes_memory` exempts
  `Guard`, `ArrayLoad` and `ArrayLength` but **not** `Store`, so any field
  write, array write, call, allocation, `LoadStatic` or monitor in the body
  refuses it.

The same fact bounds §3's per-block dominance proof. Its clause 2 needs the
allocation in the block, because a hoisted allocation is one object reused
across iterations, and then a load can see the previous iteration's store.

### 5.6 Exceptions

Handler bodies are not compiled (§3), so there is no intra-method exception
control flow for the analysis to model. This is a *dependency*, not a proof. It
is recorded here so that whoever enables handler compilation knows to revisit
`program_order_proves_dominance`.

---

## 6. Edits required outside `jit/src/escape_analysis.rs`

> **Status.** 6.1 **landed** (see §3). 6.2 **landed** for
> the identity ops (§4) and for `MonitorEnter` / `MonitorExit` (see
> `docs/jit/lock-elimination.md` §0). 6.3 **landed as a producer**:
> `ea_mark_cold_from_branch_hints` feeds `Graph::cold_nodes` from the IR branch
> hints, but `PartialEscape` still has no acting consumer (§2). 6.4 was not
> re-verified for this update. Separately, §7 step 5's "`FrameState::caller`
> is hard-coded `None` in both backends" is no longer true of the single-pass
> backend, whose deopt-point publisher records `inline_caller_chain()`. The IR
> lowerer's `caller_chain_for` still yields `None`, because IR-tier splicing
> registers no inline scope. §8 item 1 is resolved by 6.1.

All in `jit/src/lib.rs`, which is another agent's file. Cited by symbol, not
line, because that file is being edited concurrently.

### 6.1 Use the per-load answer — **DONE**

Landed as written below. `plan_scalar_replacement` now builds `load_plans` from
`info.load_value(ea_load)` and returns `None` on `LoadResolution::Unknown`
(`jit/src/lib.rs:10353`); the `LAST-WRITE-WINS HAZARD` block and its `s > l`
refusal are gone; `field_index` stayed, as predicted. Witness:
`ea_load_before_a_later_store_forwards_the_pre_store_value` (`lib.rs:17228`).
The prescription is kept below for the record.

The shape it replaced:

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

## 7. Elision for snapshot-named allocations

### Status: it fires by default

See `allocation-elision-never-fires-by-default-FIXED.md`. Until then
`IrBuilder` snapshotted at every bci, so every `Op::New` was snapshot-named.
With no descriptor by default (`CRATONVM_SCALAR_DEOPT` off), allocation elision
never fired on builder-produced IR: loads were forwarded, stores died, and the
`New` survived. Four changes in `try_compile_inner` (`lib.rs`) close that:

1. **Dead snapshot locals are cut** (`ir_prune_dead_snapshot_locals`). A local
   the bytecode cannot read again before writing it is not part of the frame
   an interpreter resumes into, so its slot becomes `NO_NODE`. The cut skips
   methods with an exception table, loop headers (an OSR entry seeds from those
   snapshots), and anything with subroutines.
2. **Only consultable snapshots pin an allocation.**
   `ea_consumable_snapshot_names` counts a snapshot only when it sits where a
   deopt can arrive. That means the resume bci of a node that can transfer to
   the interpreter (`!op_cannot_deopt`, mapped through the spliced ranges), or
   a join. The candidate's own allocation, stores and loads are treated as
   already gone. After escape analysis, `ir_prune_unconsumable_snapshots` drops
   every other snapshot. A graph whose deopting nodes cannot all be attributed
   to a bci keeps the old rule.
3. **The recipe is on whenever precise resume is.** When a consultable snapshot
   does name the object, the elision needs a `VirtualObject` recipe.
   `scalar_deopt_descriptor_available()` is `deopt_real_enabled()`. With
   precise resume off, the allocation is kept; nothing relies on the
   whole-method re-run. The lowerer's recipe now also accepts an allocation and
   stores in the SAME block as the deopt when they precede its first deopt site
   in node-id order, and it finds the deopt block of any transferring node, not
   just guards and divisions.
4. **Null checks on a fresh allocation fold** to a constant before the
   optimizer (`ir_fold_null_checks_on_fresh_allocations`). Otherwise the compare
   is both an `EaOp::Other` escape and a value use.

A backstop remains. If the lowerer still cannot describe an elided object at
some deopt point of a side-effecting body, the IR artifact is discarded and the
single-pass body, which keeps the allocation, is used
(`ir_artifact_names_an_undescribable_elided_object`).

### The history: what it took, in order

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

---

## 10. Box/unbox fold (opt-in)

`ir_fold_unbox_of_fresh_box` (`jit/src/ea_ir_bridge.rs`) is a narrow elision that does not
need the control edges §9 asks for. It targets `Integer x = i; ... x.intValue()` and the same
shape after a generic helper is inlined. That code builds an `Op::Call(Integer.valueOf)`
whose value readers are all `Op::Unbox { IntValue }` (or `Long.valueOf` / `LongValue`).

**The rule.** If every value use of the box is such an unbox, the box's identity is
unobservable: nothing compares it, publishes it, locks it or passes it on. The fold then
forwards each unbox to the primitive `valueOf` was given and deletes the call. The JLS
guarantees `Integer.valueOf(v).intValue() == v`, whatever the cache holds.

**Deopt.** A deopt is the one other reader of the box. The fold uses the scalar-replacement
planner's test (`ea_consumable_snapshot_names`) and refuses a site whose box is live at any
point a deopt can still resume at. Inside a splice no snapshot is recorded, so the test falls
back to "any snapshot names the box", which refuses.

**Measured and controlled.**
- `BoxProbe bu` (`Integer a = i + 1000; s += a.intValue();`, 10 M iterations) went from about
  650 ms to 4 ms. HotSpot takes about 2 ms.
- Kill switch: `CRATONVM_NO_IR_BOX_UNBOX_FOLD=1`.
- With the IR stage report flag on, `[ir] box-unbox fold: N` counts the folded sites.

**Still open, and the target moved.** A box that escapes pays a helper allocation per
`valueOf`. On the  `dev` tip `BoxProbe box` took 1 208 ms against HotSpot
25.0.4's 48; pass memoized the three per-box layout lookups and took it to 744.

"Inline the allocation in compiled code" was the standing plan for the rest, and  it was priced rather than started. The helper is 21.26 % of the row and the
TLAB closure 15.15 %, so the plan targets 36 %. The former per-object process-global
loads and shared counter updates have since been retired: the tripwire is checked once
per OS thread, the TLAB diagnostic flag is latched at refill, and VM allocation bytes
are published in bounded per-thread batches. An inline emission therefore has to be
re-priced against that lower baseline. The process-global/counter finding is fixed
and retained as historical evidence in
internal project records.
The page that used to carry this is
internal project records
(retired opt-in).

---

## 11. Allocations EA must not remove, and one fence it may (opt-in)

**Finalizable and `java.lang.ref` allocations stay real** (lane hFin, commit `6fd9fe2e5c`;
correctness, unconditional). The VM registers a finalizable object with the finalizer queue,
and a `Reference` for discovery, at its allocation. A scalar-replaced `new` allocates nothing:
`finalize()` never runs, and a scalar `WeakReference` keeps its referent in a frame slot, a
strong root, so `get()` after `System.gc()` answers an object HotSpot cleared. HotSpot C2 marks
both GlobalEscape. `is_elidable_construction` and the splice resolver's constructor screen
already refused a class with a finalizer, but the screen asked only `has_finalizer`, not the
reference kind; since the `jdk-references` alignment row both modes run the JDK `Reference`
constructors, so a spliced `WeakReference` constructor chain left `new WeakReference<>(o)`
replaceable. Now:

* `ir::Graph::finalizable_new_classes` lists the classes whose own `new` rows resolved with
  `has_finalizer` (a `finalize()` override, a reference kind, or not yet initialised), fed by
  `jit/src/lib.rs` from the resolved and the uninit-guard rows;
* `ea_ir_bridge::escape_analysis_from_ir` maps an `Op::New` of such a class to an escaping
  `EaOp::Call`: never a candidate, and anything stored into it (the referent) escapes globally.
  `ir_keepalive_is_local_use` excludes them too;
* `ir_optimize::eliminate_write_only_stores` keeps stores into them (`finalize()` and the
  collector read those fields).

Probe `R20HFinFinalizer`. The spliced-`new` row carries no finalizer bit, so the
`CRATONVM_JIT_IR_SPLICE_REFUSES_FINALIZABLE_NEW=0` arm is unscreened; that is the open page
internal project records.

**A `reachabilityFence` of a fresh finalizer-free allocation is a local use** (lane qLoop,
W4L3-6; `CRATONVM_JIT_EA_FENCE_LOCAL_USE`, default ON, `=0` restores the `Call` mapping). The
bridge emits the new `escape_analysis::Op::KeepAlive` only for an `ir::Op::KeepAlive` whose
referent is directly an `Op::New` of a class the compile resolved with no finalizer
(`ir::Graph::finalizer_free_new_classes`). The scalar-replacement walk accepts it, and the plan
retires those fences with the elided allocation. Lock coarsening's gap answer is unchanged
(`MayThrowInGap`, as for the `Call` it was).

**Capturing lambdas are allocations now** (lane qLamNew, `CRATONVM_JIT_IR_INDY_CAPTURE_NEW`):
a linked capturing `invokedynamic` site is built as `Op::New(proxy)` plus one `Op::Store` per
capture (see [`ir-tier-inlining.md`](ir-tier-inlining.md), "Admission after optimization pass waves
7-12"). Escape analysis and deopt rematerialisation are class-agnostic, so a lambda that does
not escape is a candidate like any other allocation.

These three are built and await their host A/B (record section 9).

## 12. Fresh objects in loops, splices and `getClass` 

Record section 13. All three are default-ON switch rows (`=0` kills) awaiting the `w17s` host A/B.

**Loop accumulator fields of a fresh object that escapes after the loop** (lane pAtom2;
`CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE`, needs `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`). EA keeps an
object that is returned after the loop, and the loop field forward leaves allocations of the compile
to EA, so `a.v += ..; a.w += ..` stayed two loop-carried memory chains (`R20PLoopStaticBarrier`
static-read-loop 3875 ns vs HotSpot 277). `loop_field_forward::promote_fresh_loop_fields` runs after
the loop static promotion and before EA, in method-entry and OSR-door bodies. A base is fresh when it
is an `Op::New` / `NewCp` outside the loop whose every use in the whole graph is a field `Load` /
`Store` base, a memory token, a `Return` value or a pure `Cmp` / `ExactClassIs` / `ClassWordOf`:
nothing can hold or publish it before the exit. The loop must pass the static promotion's census
(one exit, nothing else that can deopt, no `Any` write); its `int` / `long` cells are read once in
the pre-header, carried in value phis and written back at the exit (`add_cell_writeback`). Refusals
print under `CRATONVM_DBG_LOOP_FIELD_FORWARD=1`; `CRATONVM_DBG_JITC` prints `[ir] loop fresh field
promote: ...`. This is promotion, not scalar replacement: the object is still allocated.

**`getClass()` of a fresh allocation inside a splice folds** (lane zLdc, under
`CRATONVM_JIT_IR_SPLICE_REF_LDC`). `x.getClass() != C.class` with every definition of `x` a `new X`
becomes `If(Const(X == C))`, so no call keeps `x` escaping (`try_fold_spliced_getclass_of_fresh`; `C`
must report only itself). `Date.compareTo` / `after` / `before` no longer leak both dates through
`getMillisOf`. See [`ir-tier-inlining.md`](ir-tier-inlining.md), "Admission after optimization pass waves
17-19".

**Capturing lambdas in a spliced body are allocations too** (lane ySplice, SPLICE20-1;
`CRATONVM_JIT_IR_SPLICE_INDY_CAPTURE`): section 11's `Op::New(proxy)` plus stores now also applies to
a linked capturing site inside a splice, so a lambda built and consumed in a spliced callee is a
scalar-replacement candidate.

## 13. Reference compares of a confined allocation

`CRATONVM_JIT_EA_REF_COMPARE_FOLD`, default ON (`=0` off; §16). §8 item 6 described this
as stricter than C2; with the switch on it is not. `escape_analysis::find_confined_ref_compare_folds` reports a live
`Op::RefCompare` when one operand is provably allocation `A` on every path (`ref_operand_origin`),
`A` is `NoEscape` in the converged connection graph, and the other operand is neither `A` nor a node
whose points-to set contains `A`. A `NoEscape` object can only be reached through φ copies and loads
of confined holders it was stored into, and both put it in the copy's points-to set; every other
route needs `A` to have reached a call, a static, an unnameable holder or an unmodelled node, which
is an escape. So the compare is `false` on every execution (`A` is never null).
`ea_ir_bridge::ir_fold_confined_ref_compares` folds the IR `Op::Cmp(Eq | Ne)` to `Const(0 | 1)` right
after `analyze_escapes` in each EA round, and the round counts the folds as applied work, so the
next round sees the object without its identity observation. Tests:
`escape_analysis::r21_ea_ref_compare_fold_tests`, `ea_ir_bridge::r21_ea_ref_compare_fold_bridge_tests`;
probe `tools/probes/jit/r21/R21EaRows.java` (`ref-eq-*` rows, `time ref-compare-fresh`).

## 14. Box webs, fresh type checks, and the disable switch

See "pass: what was built" in internal project records.
`CRATONVM_JIT_IR_BOX_PHI_WEB_FOLD` (default ON, `=0` kills; the box is rebuilt without
Java, `CRATONVM_JIT_DEOPT_BOX_NO_JAVA`) turns `Integer`/`Long` boxes carried through φs and read
back only by their unbox into primitive φs; a deopt that names one rebuilds it with the wrapper's
`valueOf` (auto-box atype marker `deopt::AUTO_BOX_INTEGER_ATYPE` / `AUTO_BOX_LONG_ATYPE` on the
`VirtualObjectState`), which hands back the cached instance for a cached value, as HotSpot does.
`CRATONVM_JIT_EA_FRESH_TYPECHECK_FOLD=1` (opt-in) folds a PASSING `instanceof` / `checkcast` of an exact
allocation (never a miss: stamp admission) before each EA round, so the type check no longer escapes the object (`EaOp::Other`).
`CRATONVM_DISABLE_SCALAR_REPLACEMENT=1` now reaches this tier
(`CRATONVM_JIT_IR_HONOUR_DISABLE_SCALAR_REPLACEMENT`, default ON, `=0` the old split).

## 15. Unused small arrays and fresh array lengths

Probe 	ools/probes/jit/r21/R21AllocLean.java. The optimization passes below are available:

* `CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM` (`escape_analysis::unused_array_elim_enabled`): escape analysis deletes
  a primitive array of constant length at most 64 whose every use is `Dead`. A deopt frame state names the
  array as a `Guard` input, which is not `Dead`, so a deopt-live array is kept. The deletion drops a possible
  `OutOfMemoryError` for such arrays, as C2's `EliminateAllocationArraySizeLimit` does.
* `CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD` (`ir_optimize::fold_fresh_array_lengths`): `ArrayLength(NewArray(Const n))`
  folds to `Const(n)` for `0 <= n <= i32::MAX`, skipping an `ArrayLength` that anchors an exception edge; the
  `newarray` itself still runs, so its `OutOfMemoryError` / `NegativeArraySizeException` stay.


## 16. A `putstatic` the next store overwrites (cf-new-three close, )

Page internal project records.
A reference `putstatic` publishes its value (`EaOp::Other`), so `sink = f; sink = g; sink = h;` made all three
objects `GlobalEscape`, although only `h` can ever be read. `ir_optimize::eliminate_overwritten_static_stores`
(`CRATONVM_JIT_IR_OVERWRITTEN_STATIC_STORE`, default ON, `=0` off) runs in the scalar clean-up before escape
analysis and removes a non-volatile `Op::StoreStatic` when the next memory node is a store to the same static
cell under the same control anchor, the first store has no other user and no frame state names it, the class
was initialised at compile time (`Graph::direct_static_fields`), and no node built between the two can read
memory, raise or leave the block. With the stores gone, the identity compares `f != g` and `g != h` are the
only uses left, and §13's fold removes them: two of the three objects are scalar-replaced, as C2 does
(`R21AllocCompletableFuture cf-new-three` 52 -> 14 ns, HotSpot 4; the rest is one allocation per iteration).
Tests: `ir_optimize::tests::an_overwritten_static_store_*` and its three neighbours.
