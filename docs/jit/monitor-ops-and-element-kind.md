# Monitor ops in the IR, and the element type in the alias class

Two finished subsystems were unreachable for the same reason: `ir::Op` had no
monitor variant. `ir::MemEffect::monitor_enter`'s own doc said it —
*"`monitorenter` has no IR lowering, so a synchronized method bails to the
single-pass backend"* — and `docs/jit/lock-elimination.md` §6.2 named that bail,
not the missing bridge arm, as the real gate on lock elision and coarsening.

This change lands the ops and closes a second, unrelated trust gap in the same
model: the vectorization gate's GC-reference refusal.

---

## 1. What landed in `jit/src/ir.rs`

### 1.1 `Op::MonitorEnter` / `Op::MonitorExit`

Inputs `[ctrl, mem, obj]`, exactly the shape §6.2 specifies. The node **is** the
new memory token, like `Op::Store`: it consumes the prior token at slot 1 and
later accesses chain off it.

Registered in the one memory-shape table, `Op::memory_shape`, as
`(min_full_arity: 3, access: MemAccess::MonitorEnter | MonitorExit)` with
`token_slot: 1`. That single registration is what `memory_token_slot`,
`is_memory_token_slot` and `ir_verify::is_memory_token_input` all read, so the
token convention and the effect classification cannot disagree.

`MemAccess` gained `MonitorEnter` / `MonitorExit`. `access_location` reads the
locked reference out of slot 2 into `AliasClass::Monitor { obj }`, degrading to
`AliasClass::Any` — the refusing direction — on a short layout.

### 1.2 The effect

`effect_of_node` routes both through the constructors that already existed, so
there is still exactly one statement of the ordering discipline:

| | reads | writes | order | safepoint | allocates |
|---|---|---|---|---|---|
| `MonitorEnter` | `Monitor { obj }` | `Monitor { obj }` | `Acquire` | yes | no |
| `MonitorExit` | `Monitor { obj }` | `Monitor { obj }` | `Release` | yes | no |

`MemEffect::monitor_enter(obj)` now delegates to a new
`MemEffect::monitor_enter_of(class)` (same for exit), which is what
`effect_of_node` calls after resolving the reference from the edge list. The
`NodeId` entry points stay: a caller holding a reference and no node is the case
the discipline was first written for.

Both flags are load-bearing:

* **read *and* write** — two enters of the same object never commute.
* **safepoint** — the runtime may block here and an interpreter frame may be
  rebuilt from it. This is independent confirmation of lock-elimination §3's
  deopt argument: monitors *are* deopt points, so a coarsened region's
  boundaries move deopt points.

Neither op is `is_control` nor `is_pure`, so GVN will not fold them. **DCE is a
separate problem — see §3.1.**

### 1.3 The element kind is now in the alias class

`AliasClass::ArrayElem` gained `elem: MemKind`, filled by `access_location` from
the accessing op's own type tag (`Op::ArrayLoad(k)` / `Op::ArrayStore(k)`) via
the new private `array_elem_kind`. `Graph::element_kind(NodeId)` is the
node-level spelling of the same fact, for a caller that holds an id and not a
class.

Why in the class rather than beside it: `x64::simd_analysis`'s `vector_gate`
refuses a vector access to `MemKind::Ref` elements
(`VecRefusal::GcReferenceAccess`) because a vector store of oops bypasses the GC
write barrier. That refusal read `VecBodyOp::elem` — a **producer-supplied**
field. A barrier elision that is only as sound as whoever filled in a struct
field is the same family that produced the G1 use-after-free on this branch.
Reading the kind off the node makes the refusal unforgeable.

It also buys precision. A Java array's component type is fixed at allocation, so
two accesses at different element kinds cannot name the same object, even when
neither reference's provenance is known. `Graph::may_alias` now says so:

```rust
let kinds_disjoint = x != y && ke != kf;
!kinds_disjoint && !i.provably_distinct(j) && self.refs_may_alias(x, y)
```

The `x != y` guard is deliberate. On an ill-typed graph where *one* array node is
accessed at two kinds the references must-alias, and the model has to keep saying
so rather than treating two inconsistent type tags as a proof of disjointness.

---

## 2. Required companion edits (blocking — the workspace is red without them)

`ir::Op` has exactly **one** exhaustive `match` in the workspace, and
`AliasClass::ArrayElem` exactly one field-binding literal outside `ir.rs`.

### 2.1 `jit/src/ir_verify.rs` — `expected_arity`, line 611 (blocks the *library* build)

The match is exhaustive; it ends `Op::Dead => (0, 0),`. Insert immediately before
that line:

```rust
        // [ctrl, mem, obj]
        Op::MonitorEnter | Op::MonitorExit => (3, 3),
```

### 2.2 `jit/src/x64/simd_analysis.rs` — the `cell` fixture, ~line 2088 (blocks `cargo test`)

```rust
        fn cell(array: NodeId, index: NodeId) -> AliasClass {
            AliasClass::ArrayElem {
                array,
                index: AccessOffset::Dynamic(index),
                elem: MemKind::Int,
            }
        }
```

`MemKind` is already in scope through `use super::*`.

---

## 3. Required for correctness (compiles either way — silently wrong without)

### 3.1 `jit/src/ir_optimize.rs` — `eliminate_dead_nodes`, ~line 2080

The DCE root filter is
`matches!(n.op, Op::Return | Op::Store(_) | Op::Call { .. } | Op::ArrayLoad(_) | Op::ArrayStore(_))`.
A monitor whose token has no consumer is **not** in it, so DCE would delete it —
lock elision performed by a liveness sweep, on an escaping object, with no plan
and no balance check. Add:

```rust
                    | Op::MonitorEnter
                    | Op::MonitorExit
```

A monitor is observable in three ways no value consumer witnesses: an unbalanced
pair throws `IllegalMonitorStateException`, the lock is a happens-before edge for
other threads, and a live monitor is an identity observation that blocks scalar
replacement.

### 3.2 The two private copies of `memory_token_slot`

Both have `_ => return None` catch-alls, so they compile — and both now
*disagree* with `Op::memory_shape` about the monitors, classifying slot 1 as a
**value** input. That is precisely the drift the single table was built to end.

* `jit/src/ir_optimize.rs:1683` `memory_token_slot`
* `jit/src/lib.rs:9755` `ea_memory_token_slot`

Minimal fix, in each:

```rust
        Op::MonitorEnter | Op::MonitorExit => 3, // [ctrl, mem, obj]
```

Better fix, in each: delete the private table and call
`crate::ir::memory_token_slot(node)`, which is what `ir_verify` already does.
`ea_memory_token_slot` is the one that matters most — lock-elimination §6.1's
`value_used` check is built on `ea_is_memory_token_slot`, so an unfixed copy makes
every monitor's token consumer look like a value use and refuses every elision
plan.

### 3.3 `jit/src/ir_lower.rs:2704` — the lowering match

Its `_ => {}` catch-all means a monitor node would emit **nothing**. No monitor
node can reach the lowerer today (`ir.rs`'s builder has no `0xc2`/`0xc3` arm, so
a synchronized method still bails to single-pass at the opcode), but the arm
should be made explicit — either a real lowering or an explicit bail — before any
front end emits one.

### 3.4 `jit/src/ir_verify.rs` — precision, not soundness

`control_input_indices` (line 482) and `data_input_indices` (line 939) both have
catch-alls, so the monitors' control edge at slot 0 and locked object at slot 2
go unchecked. Adding `Op::MonitorEnter | Op::MonitorExit` to the first list, and
`=> (2..node.inputs.len()).collect()` to the second, closes that.

---

## 4. The lock-elimination §6.2 bridge, now that the ops exist

`jit/src/lib.rs`. Land **after** §6.1's per-plan elision loop, not before.

`ir_op_to_ea_op` (line 9526), before the `_ => EaOp::Other` catch-all:

```rust
        ir::Op::MonitorEnter => EaOp::MonitorEnter,
        ir::Op::MonitorExit => EaOp::MonitorExit,
```

`escape_analysis_from_ir`'s second pass (the `ea_inputs` match, ~line 9376),
before the verbatim `_ =>` arm:

```rust
            // EA reads the locked object at input 0 (`find_identity_observations`,
            // `find_lock_elisions`); the IR node carries `[ctrl, mem, obj]`, so
            // forwarding verbatim would attribute the monitor to the control edge.
            ir::Op::MonitorEnter | ir::Op::MonitorExit if ir_node.inputs.len() >= 3 => {
                vec![map_id(ir_node.inputs[2])]
            }
```

This is the same re-packing the identity-hash arm directly above already does.

---

## 5. The vectorization gate, now that the kind is in the class

`jit/src/x64/simd_analysis.rs`, `classify_body_op` (~line 1762). Replace the
producer-supplied read:

```rust
        let elem = match op.elem {
            Some(e) => e,
            Option::None => return Err(VecRefusal::UnstructuredMemoryAccess(op.node)),
        };
```

with the model-supplied one:

```rust
        let elem = match class {
            AliasClass::ArrayElem { elem, .. } => elem,
            _ => return Err(VecRefusal::UnstructuredMemoryAccess(op.node)),
        };
```

which also subsumes the `!matches!(class, AliasClass::ArrayElem { .. })` check
three lines above. `VecBodyOp::elem` then becomes fixture-only and can be
deleted once the production path builds its body descriptions from real nodes;
`Graph::element_kind(node)` is there for the path that has an id and no class.

The `MemKind::Ref` refusal itself is unchanged. What changes is that it can no
longer be defeated by a caller that fills the field in wrong.

---

## 6. To reconcile

1. **`PreheaderGuard::TripCountAtLeast { term, minimum }` is not missing.** It is
   at `jit/src/scev.rs:1206`, emitted from `scev.rs:2001`, and covered by four
   tests. The premise that it needed adding is stale.
2. **The memory model had no tests before this change.** `Op::memory_shape`'s
   doc claims *"the anti-drift test in this file asserts all of them still
   agree, op by op and arity by arity"* — there was no such test, and no test of
   `may_alias`, `may_reorder` or `MemEffect` anywhere in the workspace. The new
   `the_memory_shape_table_is_unchanged_for_every_pre_existing_op` covers the
   `ir.rs` half. The claim in that doc will only be true once the three private
   copies are either deleted (§3.2) or asserted against.
3. **The monitors are still unreachable from production IR.** The builder bails
   at `monitorenter`, so nothing emits these nodes yet. Lock elision and
   coarsening therefore remain unreachable until §4 *and* a front-end lowering
   land — a benchmark showing "no change" is not evidence of "no effect".
4. **Element-kind disjointness is a new premise in `may_alias`.** It is stated
   at its arm, like the cross-kind premises: a Java array's component type is
   fixed at allocation. Any future op that reaches `MemAccess::ArrayRead` /
   `ArrayWrite` without a `MemKind` on the op degrades to `AliasClass::Any`
   rather than guessing.
