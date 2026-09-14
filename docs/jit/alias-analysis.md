# Alias analysis: the oracle, the audit, and what is still assumed

The vectorization, range-analysis and scheduling lanes all need one question
answered — *"may these two memory operations be reordered?"* — and the review
named sound alias metadata as their prerequisite. This document is the answer in
one place:

* §1 names the **one oracle** those lanes must ask, and states its contract and
  the premises its every "no, they do not alias" answer rests on.
* §2 is the **audit** of the two analysis passes that answer alias questions
  themselves (`jit/src/ir_optimize.rs`, `jit/src/escape_analysis.rs`), question
  by question, with the proof each relies on and whether that proof is
  established.
* §3 is the **escape-invalidation set**: everything that can make a
  "does not escape" conclusion wrong, and where each is checked.
* §4 is what remains **unvalidated** — assumptions that are load-bearing today
  and that nothing tests.

Three defects were found and fixed; they are marked **FIXED** in §2 with the
regression test that fails without the fix. All three are the same shape as the
miscompilation this branch already paid for (escape analysis forwarding a field
load to a *later* store's value): **a may-alias answer used where a must-alias
answer was required.**

---

## 1. The oracle

**There is one. Do not build a second.** It lives in `jit/src/ir.rs` and it is:

```rust
Graph::may_alias(&self, a: AliasClass, b: AliasClass) -> bool
Graph::may_reorder(&self, earlier: NodeId, later: NodeId) -> Reorder
Graph::may_reorder_effects(&self, earlier: MemEffect, later: MemEffect) -> Reorder
```

`Reorder` carries the *reason* — a `ReorderProof` naming the fact that licenses
the move, or a `ReorderBlock` naming the fact that forbids it. Record the proof;
do not re-derive it. A pass that invents its own reasoning is a fourth private
notion of aliasing, which is the exact situation the model was built to end
(`jit/src/ir.rs`, the section header at the `AliasClass` definition, enumerates
the three copies that existed before it).

### 1.1 Contract

| | |
|---|---|
| **Direction** | `may_alias` and `may_reorder` return the **conservative** answer on any doubt. `true` from `may_alias` means "may"; `false` is a *proof* of disjointness. `Reorder::Blocked` may be pessimistic; `Reorder::Allowed` may not be wrong. |
| **Degradation** | An unreadable node layout classifies as `AliasClass::Any`, never `AliasClass::None`. A node id that names nothing answers `MemEffect::OPAQUE`. Failure moves *up* the lattice. |
| **Scope** | Memory only. It does **not** answer control dependence, and it does **not** answer implicit-exception order — `Op::Load`/`Op::Store`/`Op::ArrayLoad`/`Op::ArrayStore` fault on a null base or an out-of-range index and are deliberately *not* flagged `MemEffect::safepoint`. A scheduler must combine this answer with control dependence; it must not use it alone. |
| **Arity** | **Pairwise.** `may_reorder(a, b)` says nothing about a third node between them. Moving `b` above `a` past an intervening `c` requires the query to hold for `(c, b)` too. |
| **Offsets** | Use `Graph::memory_effect(id)`, not the free `effect_of_node`. Only the graph-aware form constant-folds a `Dynamic` offset into a `Const`, and `AccessOffset::provably_distinct` proves disjointness **only** for two unequal compile-time constants. Two runtime values may be equal; a runtime value may equal any constant. |

### 1.2 The premises behind every `false`

Each of these is a claim about the VM's object layout, not a fact about the IR.
If one is broken, `may_alias` silently starts licensing reordering across a real
dependence.

1. **Different storage kinds never overlap.** A field cell is not an array
   element (a Java object is either an array or a class instance, and verified
   bytecode never reaches `getfield` on an array); an array's length word is not
   a field cell; a class's static storage is a per-class area no object
   reference addresses; an object's monitor is none of them.
   *Obligation:* any new op with a computed/raw address (an `Unsafe`-style
   access) must classify as `AliasClass::Any`, **not** as one of the structured
   classes.

2. **`AliasClass::ArrayLength` is read-only storage.** Nothing writes an array's
   length after the allocation that produced it, so a length read never
   conflicts with a write. This is what lets an `ArrayLength` commute with a
   store, which the token chain (where it sits between two writers) cannot.

3. **Same kind, same base ⇒ disjoint only for two unequal constant offsets.**
   `AccessOffset::Absent` — the layouts that carry no offset operand at all — is
   *"the object's storage"*, and is therefore **never** disjoint from another
   access to the same base. This is a may-alias. It was the source of audit item
   A3.

4. **Same kind, different base ⇒ disjoint only when the bases are provably
   different objects** (`Graph::refs_may_alias` → `RefOrigin::provably_distinct`:
   both sides must have known provenance, their allocation sets must be
   disjoint, and at most one may be possibly-pre-existing, because two
   parameters can be the same object — `foo(x, x)`).

5. **Two array elements at different element types never overlap.**
   `AliasClass::ArrayElem { elem }` distinguishes an `int[]` cell from an
   `Object[]` cell without needing either reference's provenance, because a Java
   array's component type is fixed at allocation.

   **This is the premise the parallel lanes must not break, and it is the one
   most easily broken.** It is sound only while *every* `Op::ArrayLoad` /
   `Op::ArrayStore` carries the array's **component type** as its `MemKind`.
   The guard is written as `x != y && ke != kf` (`Graph::may_alias`): two
   accesses at different kinds through the *same* array node keep must-aliasing,
   so an ill-typed graph cannot manufacture a disjointness. But two accesses at
   different kinds through *different* nodes are declared disjoint outright.

   A **widened or vectorized access that keeps a different `MemKind` than the
   array's component type breaks this** — e.g. rewriting eight `byte[]`
   accesses as one `MemKind::Long` access would make that access provably
   non-aliasing with every remaining `MemKind::Byte` access to the same array.
   Any lane that widens an element access must either keep the component
   `MemKind` or degrade the class to `AliasClass::Any`.

   The tag is read off the accessing op by `array_elem_kind`, never supplied by
   a caller, which is what makes `x64::simd_analysis`'s refusal of vector
   accesses to `MemKind::Ref` elements (a vector store of oops bypasses the GC
   write barrier) unforgeable. Reading it from the class rather than carrying a
   private copy is a requirement, not a style preference.

6. **Ordering is separate from aliasing.** Two monitor operations on provably
   distinct objects do not alias and still do not commute, because
   monitor-enter is an `Acquire` and monitor-exit a `Release`. Anything that
   asks only `may_alias` and skips `may_reorder_effects` loses the JMM fences,
   the allocation-order rule, and the safepoint rule.

### 1.3 What `may_reorder_effects` applies, in order

1. Either side inert → `Allowed(EffectFree)`.
2. Either side a full fence (`SeqCst`, i.e. a volatile write) → `Blocked(Fence)`.
3. `earlier` has acquire → `Blocked(Acquire)`: nothing after a monitor-enter or
   a volatile read may move above it.
4. `later` has release → `Blocked(Release)`: nothing before a monitor-exit or a
   volatile write may move below it.
   *3 and 4 are one-sided on purpose* — that asymmetry is the JMM's roach motel,
   and it is why a synchronized region bounds the accesses inside it without
   pinning the ones outside.
5. Both allocate → `Blocked(Allocation)`. Which allocation runs first is
   observable through `OutOfMemoryError` and through the addresses and identity
   hashes the collector hands out.
6. Locations may conflict (W/R, R/W, W/W) → `Blocked(MayAlias)`.
7. One is a safepoint and the other writes → `Blocked(Safepoint)`. The
   constraint is specifically about **writes**: every store the method has
   performed must have happened before an interpreter frame is rebuilt, and no
   store it has not yet performed may have. Loads cross a safepoint freely.
8. Neither writes → `Allowed(ReadOnly)`.
9. Otherwise → `Allowed(DisjointLocations)`.

---

## 2. Audit

Scope: every place in `jit/src/ir_optimize.rs` and `jit/src/escape_analysis.rs`
that answers *"can these be reordered / is this load redundant / is this store
dead / is this object confined"*.

| # | Question asked | Proof relied on | Established? | Action |
|---|---|---|---|---|
| A1 | LICM: may this loop-invariant `Op::Load` be computed once in the pre-header? (`licm`) | The load's base and address are invariant; the loop contains **no** memory operation other than field `Load`/`Store` and φ (`loop_has_hard_barrier` — `Call`, `New`, `NewArray`, `Guard`, `MonitorEnter/Exit`, `ArrayLoad`, `ArrayStore`, `ArrayLength` are all impure, non-control, and outside the exempt list, so each disqualifies the whole loop) | **Yes.** Verified against `Op::is_pure` (`ir.rs:429`, a whitelist of arithmetic only) and `Op::is_control` (`ir.rs:421`). Every memory op that is not `Load`/`Store` falls through to "hard barrier". | none |
| A2 | LICM: does an in-loop `Store` clobber this load? (`loop_store_clobber` / `load_safe_past_clobber`) | Every in-loop store's base resolves (cross-φ) to a set of *fresh in-method allocations* with no pre-existing component; the load's own allocation set is disjoint from that set. The pre-existing (parameter) component of a load base is always safe, because no store to a fresh local allocation can clobber an object that existed before the method ran | **Yes**, and it is the same judgement `RefOrigin::provably_distinct` makes. Field-**in**sensitive (base only), which is the conservative direction. An unknown-provenance base (loaded ref, call result, over-wide φ) is never safe | none |
| A3 | LICM: is the loop body complete? (`loop_body`) | The forward control set ∩ "reaches a back edge", plus a sweep pinning non-pure nodes whose input is in the body | **NO — the sweep was one arena pass in id order**, which is complete only if every node's inputs sort before it. That is a premise about the *producer* (forward-flowing control/memory edges), not a property of the IR, and a back-patched edge breaks it. A missed node is a body `loop_has_hard_barrier` and `loop_store_clobber` never see — the under-approximation direction, which is the one that licenses an unsound hoist | **FIXED**: sweep runs to a fixed point. Test `test_loop_body_pins_a_node_whose_only_link_sorts_after_it` |
| A4 | DSE: is this store overwritten before any read? (`eliminate_dead_stores`, phase 1) | Node-id order is a valid before/after relation, because every control node and every φ is impure and therefore flushes the pending set — so two stores only ever match inside one straight-line region | **Yes.** Checked op by op: `If`, `Merge`, `Region`, `Proj`, `Start`, `Return`, `Phi`, `Guard` are all outside `Op::is_pure`, so `is_memory_barrier` returns true for each | none |
| A5 | DSE: do these two stores write the **same cell**? (the `StoreLoc` key) | `(base node id, offset node id, MemKind)` structural equality | **NO.** `MemKind` is an access *width*, not a field index — two distinct `int` fields share it. In the two layouts that carry no offset operand (compact `[base, value]`, and `[ctrl, mem, base, value]`) the key degenerates to `(base, NO_NODE, kind)`, so `o.x = 1; o.y = 2;` matched and the **live** `o.x = 1` was deleted. This is precisely `AccessOffset::Absent`, which `ir.rs` defines as a **may**-alias; a deletion needs a must-alias. Latent, not live: the production `putfield` lowering (`ir.rs:5040`) always emits the 5-input form with a distinct `Const(field_index)` offset | **FIXED**: `store_matchable_location` — an absent offset is matchable only when the base allocates an object with exactly one field, where "the object's storage" *is* field 0. Test `test_dse_absent_offset_stores_to_a_multi_field_object_are_not_matched`; the positive control is `test_dse_offset_distinguished_stores_still_match_and_still_separate` |
| A6 | DSE: does this intervening `Op::Load` observe a pending store? | Every pending store targets a fresh local allocation, and two distinct fresh allocations never alias; so a load of a *different* local allocation flushes only its own base, while a non-local or unreadable base flushes everything | **Yes.** The `is_local_alloc` gate is a direct `Op::New`/`Op::NewArray` test on the base node — no φ, no transitive resolution — so equal base ids are the same allocation site, and the enclosing straight-line region (A4) makes it the same *object* | none |
| A7 | DSE: is every store to this allocation unobserved? (`eliminate_write_only_stores`, phase 2) | A whole-graph property: no `Op::Load` addresses the allocation, and the allocation never appears anywhere except as the *base* slot of a `Store`. Any other reference — a `Call` argument, a `Return`, a `Store` **value** slot, an `ArrayLength`, a φ, a `Proj`, an unreadable layout — disqualifies it | **Yes**, and it is path-insensitive by construction, so it needs no ordering relation. Field-independent, so A5 does not touch it | none |
| A8 | DSE: is it safe to delete a node the memory-token chain runs through? (`kill_store_splicing_memory_chain`) | Consumers are rewired to the killed store's *own* incoming token first. Refuses outright when the store has no incoming token to hand on, when that token is absent/out of range/already dead, or when something names the store in a **non**-token slot (including a safepoint snapshot slot, which `use_counts` cannot see) | **Yes.** The token-slot predicate delegates to `ir::memory_token_slot`, derived from the single `Op::memory_shape` table, so it cannot drift from what `ir_verify`'s ordering lane checks | none |
| A9 | EA: which store does this replaced load read? (`resolve_field_load`) | A **positional** record of every store per field, plus `program_order_proves_dominance` (no `Op::If`, no `Op::Merge`, no multi-input φ anywhere in the graph). A field with no store anywhere resolves to `ZeroDefault` regardless of control flow; otherwise the answer is the latest store with `store < load`, and a load with no preceding store reads `ZeroDefault` — never a store from its own future | **Yes.** This is the already-fixed defect and the fix holds: `apply_scalar_replacement` reads `load_values`, not `field_values`, and refuses to touch the graph at all if any load is `Unknown`. `field_values` is documented as *not* a forwarding source | none |
| A10 | EA: is a load reached through a φ reading *this* allocation? (`find_scalar_replacements`, the `Op::Phi` arm) | The φ must be `NoEscape`, its resolved points-to set must be exactly `{id}`, **and** every reference-producing input must itself resolve to exactly `{id}` | **Yes**, and the third clause is the load-bearing one: a `Param`/`Call`/`Load` input contributes no points-to entry, so a φ merging the allocation with an unknown reference would otherwise still resolve to the singleton `{id}` | none |
| A11 | EA: has the use walk seen **every** use of the allocation? | It walks the allocation's own uses plus the uses of transparent φ copies | **NO.** A reference to the object recovered by *loading it back out of its own field* (`o.next = o; Foo p = o.next; p.x = 5;`) is neither: `p` is a `Load` node, so the store through it is never visited, `can_replace` stays true, the allocation is deleted, and a real store is left writing through an `Op::Dead` holder. The `is_value` role test in the `Op::Store` arm does not catch it — `is_holder` wins the `if`/`else if` in exactly the self-store case | **FIXED**: a self-reference gate after the walk refuses the object when any recorded field-store value *may point to* the allocation (`resolve_points_to`, so the φ-aliased spelling is covered too, and order-independently). Test `an_object_stored_into_its_own_field_is_not_scalar_replaced`; negative control `the_self_reference_gate_does_not_refuse_an_ordinary_object` |
| A12 | EA: is this object confined, so its monitors may be elided? (`find_lock_elision_plans` E2) | `cg.get_escape(object).is_confined()`, computed by the fixed point in `propagate_escape_states` | **NO.** `build_connection_graph` had no arm for `Op::Other`, the bridge's catch-all (`lib.rs::ir_op_to_ea_op`), so an unmodelled node contributed **no escape at all**. Two `ir::Op`s land there and publish a reference: `LambdaIntToDouble` (`MemAccess::Opaque`, `MemEffect::OPAQUE`, a safepoint, and it *invokes the lambda it is handed*) and `Guard` (a deopt point that rebuilds an interpreter frame from the references the frame state names). Scalar replacement was safe by accident — its use walk refuses an `Op::Other` use at the catch-all arm — but **lock elision walks no uses**, so the object looked confined and every monitor on it was deleted | **FIXED**: `Op::Other` escapes its reference operands to `GlobalEscape`. Costs scalar replacement nothing (already refused); costs lock elision only where it was unsound. Test `an_unmodelled_use_blocks_lock_elision`; scope control `the_unmodelled_use_rule_ignores_dead_and_referenceless_nodes` |
| A13 | EA: are two array accesses ever separated by element type here? | — | **N/A, verified.** Neither pass reasons about array elements at all. `find_scalar_replacements` matches `Op::New` only; `field_in_range` returns `false` for every `Op::NewArray`, so array field indices always take the conservative path; and LICM/DSE treat `Op::ArrayLoad`/`Op::ArrayStore` as unconditional barriers. The `AliasClass::ArrayElem { elem }` premise is exercised only by `ir::Graph::may_alias` — see §1.2 item 5 for the obligation it puts on the vectorization lane | none |
| A14 | EA: is the fixed point's answer usable when it did not converge? | It is not; `propagate_escape_states` escalates **every** node to `GlobalEscape` at the iteration cap | **Yes.** The lattice only joins upward, so an unconverged state is an under-estimate, and both acting consumers test `== NoEscape`. `escalate_all_to_global` writes every id explicitly, because `get_escape` reports an *absent* node as `NoEscape` | none |

---

## 3. The escape-invalidation set

A "does not escape" conclusion is invalidated by anything that can make the
reference observable outside the allocating frame, or that can make its *address*
observable. Both lists are checked; they are different questions and conflating
them is how the last miscompilation on this branch happened.

### 3.1 Reachability (`EscapeState`)

| Leak | Where it is caught | State |
|---|---|---|
| `Op::Return` (`areturn`) | `build_connection_graph` | `GlobalEscape` |
| `Op::Throw` (`athrow`) — a thrown reference leaves the frame exactly as a returned one does | `build_connection_graph` | `GlobalEscape` |
| `Op::Call` argument | `build_connection_graph` | `ArgEscape` |
| Store into a field of an escaping holder — *at both `ArgEscape` and `GlobalEscape`*, because a callee that can reach the holder can reach the stored value | `propagate_escape_states`, the store rule | joins the holder's state |
| Store into a holder that resolves to **no** allocation at all — `putstatic`, a bridge gap, any opaque reference | same rule, `obj_pts.is_empty()` | `GlobalEscape` |
| Store through an `Op::Param` holder (`this.f = new …`, the lazy-init getter) | `Op::Param` is seeded `GlobalEscape` at build time, so the store rule fires for it | `GlobalEscape` |
| Malformed store (a value with no resolvable holder) | `propagate_escape_states`, the `else if` arm | `GlobalEscape` |
| **Any unmodelled node (`Op::Other`)** — `ir::Op::LambdaIntToDouble`, `ir::Op::Guard`, and everything else the bridge has no variant for | `build_connection_graph`, **added by this audit (A12)** | `GlobalEscape` |
| Flow through a φ | deferred edges, then the points-to propagation | joins |
| Non-convergence of the fixed point | `escalate_all_to_global` | `GlobalEscape` |
| **A reference recovered by loading the object out of its own field** | `find_scalar_replacements`, the self-reference gate, **added by this audit (A11)**. Note this is *not* an escape — the object stays confined — it is an **alias** the use walk cannot see, which is why the escape lattice alone cannot refuse it | object refused |

Not escapes, deliberately: `Op::MonitorEnter`/`Exit` (a monitor does not publish
the object — but see §3.2), `Op::ArrayLength`, `Op::If` (its operand is a
comparison result), `Op::Load` with the object as *holder*, and a comparison
against the null literal (a fresh allocation is non-null by construction, so
`o == null` reads no address; classifying it as an identity observation would
refuse every object that is ever null-checked).

### 3.2 Identity (independent of §3.1)

An object can be perfectly `NoEscape` and still be unreplaceable because
something observes its *address*. `find_identity_observations` collects these,
and `find_scalar_replacements` applies the gate **before** the use walk, so an
observation reached through an alias the walk does not classify as transparent
cannot slip past:

* `if_acmpeq` / `if_acmpne` — two scalar-replaced objects with equal fields are
  indistinguishable; two heap objects are not (`Op::RefCompare`).
* `System.identityHashCode`, and an `Object.hashCode` that is not overridden — a
  stable per-object value derived from the header (`Op::IdentityHash`).
* `monitorenter` / `monitorexit` — the monitor *is* the object header.

The supported route to replacing a synchronized object is two-phase: analyse,
apply the elision the analysis offers (the monitors become `Op::Dead`), analyse
**again**. Coupling the two — "the monitor will be removed anyway" — is wrong,
because `apply_ea_to_ir` independently refuses elisions this module offers (for a
lock naming a safepoint slot, for one whose memory chain cannot be spliced, for
one whose value is still read), and each refusal would leave a live
`monitorenter` on a deleted reference.

---

## 4. What remains unvalidated

Ordered by how much a wrong answer costs.

1. **The element-type premise has no negative control.** Nothing tests that a
   pass cannot introduce an `Op::ArrayLoad`/`Op::ArrayStore` whose `MemKind` is
   not the array's component type. §1.2 item 5 states the obligation; it is
   currently enforced by nobody. The cheapest guard would be an `ir_verify` lane
   asserting that all array accesses through the same array node agree on their
   `MemKind` — that catches the widening mistake at the node that made it.
   *(Cross-file: `jit/src/ir_verify.rs`, not this lane's to edit.)*

2. **LICM repoints a hoisted load's control edge but not its memory token.**
   `licm` sets `inputs[0]` to the pre-header and leaves `inputs[1]` naming an
   in-loop memory φ or store. The *value* is sound — A1 and A2 together prove no
   in-loop writer touches that load's location, so the memory state at the
   pre-header and inside the loop agree for that cell — but the graph now
   encodes a node anchored before the loop that consumes a token defined inside
   it. Whether `ir_schedule` can honour that, and whether `ir_verify`'s ordering
   lane should reject it, is a scheduling question this lane did not answer.
   *(Cross-file: `jit/src/ir_schedule.rs`, `jit/src/ir_verify.rs`.)*

3. **LICM hoists a faulting load out of a possibly-zero-trip loop.** There is no
   trip-count guard. `Op::Load` faults on a null base and deopts; the deopt
   re-enters the interpreter at the load's bci, which does not execute the loop
   body, so the fault is not *observed* — but this rests on the deopt path, not
   on anything the alias model says, and `ir.rs`'s own section header explicitly
   excludes implicit-exception order from what `may_reorder` answers.

4. **`AliasClass::Static` has no producer.** `getstatic` / `putstatic` have no IR
   lowering, so static field access reaches EA as a store to a holder with an
   empty points-to set (caught, `GlobalEscape`) and reaches the memory model not
   at all. The class exists so the lowering lands with a classification instead
   of widening everything to `Any`. Nothing exercises it end to end.

5. **`Op::Safepoint`, `Op::Throw`, `Op::RefCompare` and `Op::IdentityHash` are
   partly unproduced in the EA graph.** `escape_analysis_from_ir` now emits
   `RefCompare` and `IdentityHash` for the two shapes it can prove, but nothing
   emits `Op::Safepoint` (the IR keeps safepoints in a side table, not as
   nodes) and nothing emits `Op::Throw`. `LockRefusal::SafepointInGap` is
   therefore unreachable from production IR, and lock coarsening relies on the
   gap allowlist alone. The allowlist admits no node that can be a safepoint, so
   the outcome is right; the *reason* is incidental.

6. **A5's fix leaves a residual precision cliff that nothing measures.** A store
   in a no-offset layout to a multi-field allocation is now never matched. Since
   the production lowering always carries an offset, this should cost zero
   deletions on real code — but there is no counter that would show it if a
   future producer started emitting the compact form.

7. **`MAX_ALIAS_ALLOC_SET` (8) and `REF_ORIGIN_MAX_DEPTH` (32) are unmeasured.**
   Both degrade to unknown provenance, which is the safe direction, but nothing
   reports how often a real method hits either bound — so the difference between
   "this pass is precise" and "this pass silently gave up" is currently
   invisible.
