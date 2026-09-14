# Deoptimization metadata: what is emitted, what is proved, what is missing

Scope: the P0 items *"Complete deoptimization metadata"* and *"Emit precise oop
maps at every safepoint"* of the C2 review.

Acceptance criteria under audit:

> Any guard or dependency failure reconstructs byte-for-byte equivalent
> interpreter state.

> Moving GC at every call, allocation, poll, and deopt site preserves all
> objects and updates every reference.

Neither is met today. This document records exactly which pieces exist, what
`jit/src/deopt.rs` proves before an artifact is installed, and the ordered
list of what is still missing.

The standing rule over all of it: **if the metadata cannot describe a state
exactly, refuse.** A deopt that reconstructs a *plausible* frame instead of the
*correct* one produces a running program with silently wrong values, which is
strictly worse than a bailout — a bailout only costs the optimizing tier.

---

## 1. Completeness table

Two backends emit deopt metadata and they are at different stages, so each gets
a column. "IR" is the optimizing sea-of-nodes tier (`jit/src/ir_lower.rs`);
"1-pass" is the baseline single-pass backend (`jit/src/x64.rs`).

| Metadata element | IR | 1-pass | Where | Test |
| --- | --- | --- | --- | --- |
| Native PC → deopt point | **emitted** | **emitted** | `jit/src/ir_lower.rs:3942-3964` (`build_deopt_points`, keyed by `bci_native`); `jit/src/x64.rs:2315-2585` (`build_and_record_deopt_point`, keyed by `buf.pos()`) | `deopt.rs::unsorted_points_break_the_binary_search_and_are_rejected` |
| Inlined scope chain | **buildable, not produced** | **absent** | IR: `Lowerer::resolve_frame_state` now fills `caller` from `Lowerer::caller_chain_for` (`jit/src/ir_lower.rs`), driven by an `InlineScopeTable` (`jit/src/ir.rs`) that maps a safepoint index to its scope; `lower_inner_with_scopes` takes the table. **But the production compile path calls `lower_inner` (`jit/src/lib.rs`), which passes `InlineScopeTable::new()` — `push_scope` has no non-test caller, so every compiled method today still has `caller: None` at every point.** 1-pass: `caller: None` is still hard-coded in `build_and_record_deopt_point`; there is no scope stack pushed at the splice. So the inliner still attributes every inlined callee's frame to the caller's method key with the callee's bci. See `docs/jit/deopt-inline-scopes.md` for the producer side | `deopt.rs::inlined_caller_scopes_are_checked_too` (the verifier walks the chain); `ir_lower.rs`'s `resolve_with_scopes` tests (the lowerer builds one *when handed a table*) — neither proves a producer populates it |
| BCI | **emitted** | **emitted** | `DeoptimizationPoint::bci` + `FrameState::bci`, both set from the snapshot bci | `deopt.rs::bci_past_the_end_of_the_method_is_rejected`, `point_bci_must_match_its_frame_state_bci` |
| Locals | **emitted, typed from IR node type** | **emitted, typed from a whole-method classifier** | IR: `jit/src/ir_lower.rs:3369-3444` (`frame_value_for` / `typed_stack_slot`, driven by `IrType`); 1-pass: `jit/src/x64.rs:2347-2447` (oop mask ∪ `local_kinds`, with a per-bci refinement for `Ambiguous`) | `deopt.rs::reconstruct_resolves_typed_slots`, `more_locals_than_max_locals_is_rejected` |
| Operand stack | **emitted** | **approximated** | IR: same mapper as locals. 1-pass: `jit/src/x64.rs:2449-2525` — the abstract operand stack has **no per-entry width source**, so a non-oop slot in a method that touches any `long`/`float`/`double` is recorded `Unsupported` (refuse) unless an `invokedynamic` descriptor types it | `deopt.rs::deeper_stack_than_max_stack_is_rejected` |
| Locks / monitor state | **absent** | **partial** | IR: `monitors: Vec::new()` unconditionally (`jit/src/ir_lower.rs:3458,3782,3791`). 1-pass: only monitors on **scalar-replaced** objects whose lock was elided (`jit/src/x64.rs:2537-2545`, from `sr_monitor_at`); an ordinary `synchronized` block open at a deopt bci contributes nothing, and `can_deopt_resume` is switched off wholesale when any monitor was elided (`jit/src/x64.rs:24180`) | `deopt.rs::unbalanced_lock_state_is_rejected` |
| Constants | **emitted** | **emitted** | `FrameValue::Int/Long/Float/Double` from `Op::Const`/`Op::ConstF` (`jit/src/ir_lower.rs:3379-3395`), cat-1/cat-2 and int/FP distinguished | `deopt.rs::frame_value_int`, `reconstruct_resolves_fp_slots_and_xmm_registers` |
| Register locations | **unused by design** | **emitted** | `FrameValue::Register/RegisterLong/RegisterRef/XmmFloat/XmmDouble`, resolved against the stub-spilled `SavedRegisters`. The IR lowerer spills every value, so it never emits one | `deopt.rs::register_homed_locals_ignore_a_stale_canonical_frame_slot`, `out_of_range_register_descriptors_are_rejected` |
| Stack-slot locations | **emitted** | **emitted** | `FrameValue::StackSlot{,Ref,Long,Float,Double}(off)`, read as `*(rbp + off)` with `off < 0` | `deopt.rs::reconstruct_resolves_typed_slots` |
| Virtual (scalar-replaced) objects | **gated, partial** | **gated, partial** | IR: `jit/src/ir_lower.rs:3826-3936` (`frame_value_for_object`), only when `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`; bails to `Undefined` on nested virtuals, unproven dominance, or any unresolvable field. 1-pass: `jit/src/x64.rs:2302-2313` (`sr_virtual_object_state`) | `deopt.rs::virtual_object_graph_integrity_is_checked`, `slot_naming_a_removed_node_is_rejected` |
| Reexecute flag | **emitted, unread** | **emitted, unread** | `DeoptimizationPoint::semantics: ResumeSemantics` now exists (`jit/src/deopt.rs`, next to `frame_state`) and **every** producer stamps it — `ir_lower.rs` at all three construction sites and `x64.rs::build_and_record_deopt_point`, all with `ResumeSemantics::for_reason(reason)`, which is exactly the prose convention written down once instead of re-derived per consumer. Two flags (`reexecute`, `rethrow_exception`) give the three states `RESUME` / `REEXECUTE` / `RETHROW`. **The VM resume sink does not read it yet** — `vm/src/runtime/interpreter.rs` still infers re-execute-vs-resume from `DeoptReason`, so the field is recorded and round-tripped but changes no behaviour. See `docs/jit/deopt-frame-state-interning.md` §2 and §5.2 | `deopt.rs::frame_state_interning_tests` (semantics are part of a state's identity; `for_reason` / `for_caller_scope`) |
| Pending-exception state | **emitted as a reason + a separate stash** | same | `DeoptReason::PendingException`; the frame is published to `LAST_EXCEPTIONAL`, never `LAST_DEOPT`, precisely because it is not resumable (`deopt.rs`, `take_exceptional_frame`) | `x64_deopt_entry_tests` (stash routing) |
| Oop map per safepoint | **emitted, fail-closed** | see note | `jit/src/ir_lower.rs:803-896` (`emit_safepoint_map`) → `crate::OopMapEntry` (`jit/src/lib.rs:864-916`). Coverage is claimed only when every live `Ref` slot could be named **and** the shadow-stack push was emitted; otherwise `moving_young_coverage_complete: false`, which routes the cycle to the non-moving sweep | `deopt.rs::reference_slot_absent_from_the_oop_map_is_rejected`, `incomplete_coverage_does_not_flag_uncovered_references` |

Note on the 1-pass oop maps: they are produced by the same `OopMapEntry`
mechanism, but the deopt map and the oop map are built by **different** code
paths from **different** sources (the deopt map from `local_oop_masks` /
`stack_oop_marks`, the oop map from the register allocator's live sets). Nothing
before this change compared them. That is the gap `DeoptVerifier` closes.

---

## 2. Eliminated vs. undefined

### The bug this distinction exists to prevent

`FrameValue::Undefined` means *"the interpreter never reads this slot"* and the
resume sinks act on that: `fv_to_value` maps it to `Value::Int(0)`
(`vm/src/runtime/interpreter.rs:13237`) and `field_value_to_value` does the same
(`vm/src/runtime/deopt_materialize.rs:297`). That is sound for a local no path
has stored to, and for the reserved upper half of a cat-2 value.

But every producer bail path *also* wrote `Undefined`:

* `jit/src/ir_lower.rs:3417` — a node with no assigned machine location;
* `jit/src/ir_lower.rs:3850,3858,3878,3889,3909,3916` — **every** bail in
  `frame_value_for_object`: no deopt block, unproven dominance for the `New` or
  any field store, a nested virtual object, or an unresolvable field.

So a scalar-replaced object whose materialization recipe could not be built was
recorded as "nothing is here", and the resume rebuilt a reference-typed local as
`Value::Int(0)` — a **null where a live object was**, with no error anywhere.
That is a silent wrong reconstruction, not a refusal.

### The representation

`deopt.rs` now carries the two states separately:

| State | Variant | Resumable? | Sink behaviour |
| --- | --- | --- | --- |
| Genuinely undefined | `FrameValue::Undefined` | yes | `Value::Int(0)` — correct, because nothing reads it |
| Eliminated, no recipe | `FrameValue::MaterializationRequired(EliminatedValue)` | **no** | catch-all arms already refuse it (`fv_to_value` → `None`, `field_value_to_value` → `Err`) ⇒ safe whole-method re-run |
| Eliminated, recipe present | `FrameValue::VirtualObject` / `VirtualObjectRef` | yes | materialized on the heap by `deopt_materialize` |
| Unknown JVM width/type | `FrameValue::Unsupported` | no | refuse (unchanged) |

`EliminatedValue { producer, class_id, cause }` carries the IR node id of the
deleted producer and an `EliminationCause`
(`ScalarReplacedObject` / `NestedVirtualObject` / `EliminatedStore` /
`ElidedLock` / `Unclassified`), so the compiler report can name the pass and the
node rather than reporting an anonymous "cannot resume".

`MaterializationRequired` is deliberately **not** the same as `Unsupported`:
both refuse, but only one of them says *"an optimization deleted this"*, which
is the signal that a producer needs to start emitting a recipe for that shape.

### Producers that must be updated to populate it

None do yet — the variant is added with no producer, which is why the silent
`Undefined` path is still live. In priority order:

1. `jit/src/ir_lower.rs:3850,3858,3878,3889,3909,3916` — every `return
   FrameValue::Undefined` in `frame_value_for_object` must become
   `FrameValue::MaterializationRequired(EliminatedValue::allocation(new_id,
   info.class_id, cause))`, picking the cause each bail already knows: the
   dominance and unresolvable-field bails are `ScalarReplacedObject`, the
   nested-virtual bail at `:3909` is `NestedVirtualObject`.
2. `jit/src/ir_lower.rs:3417` — the "no machine location" fallback in
   `frame_value_for`. This one needs care: it fires both for genuinely dead
   values and for values a pass removed, and only the caller knows which. The
   sound split is: if the node is `Op::Dead`, emit
   `MaterializationRequired(EliminatedValue::new(node_id,
   EliminationCause::Unclassified))`; if the slot was `NO_NODE` in the snapshot,
   keep `Undefined`.
3. `jit/src/x64.rs:2378` — the dead-local `Undefined` push is *correct* as-is
   (it is backed by a liveness proof) and must **not** be changed.
4. `jit/src/lib.rs:6999-7035` — see §5.

---

## 3. The oop-map / deopt-map agreement rule

> **A reference the deopt map will materialise must be one the GC also knows to
> update.**

Concretely, at one safepoint:

* the deopt map names a reference-typed local/stack slot as
  `FrameValue::StackSlotRef(off)`, read as `*(rbp + off)` with `off < 0`;
* the oop map names the same word as a **positive** `i16` in
  `OopMapEntry::frame_slot_offsets`, meaning `[rbp - off]`.

So the rule is `oop_map.frame_slot_offsets.contains(-off)`, and the same for a
register-resident reference (`FrameValue::RegisterRef(r)` ⇒ the map must cover
GPR `r`).

**Why a disagreement is a moving-GC correctness bug.** If the deopt map reads a
word as an object but the oop map omits it, a relocating young collection
between the safepoint and the deopt copies the object and rewrites every root it
knows about — not that word. The deopt then hands the interpreter the *pre-copy*
address, which is a dangling pointer into from-space. This is the same class of
failure as `docs/threading/objectref-concurrency-contract.md` §4.2 "frozen
in-JIT peers", except that here the frame is *not* frozen and nobody called
`mark_moving_young_coverage_incomplete_because`.

**The exemption.** When `OopMapEntry::moving_young_coverage_complete` is `false`,
the collector must not move anything this frame can address, so an uncovered
reference cannot go stale. `DeoptVerifier` therefore only enforces agreement on
maps that *claim* completeness — enforcing it on conservatively-handled frames
would be a false positive on every frame the current backends produce
(`incomplete_coverage_does_not_flag_uncovered_references`).

The converse direction (an oop-map slot the deopt map does not name) is **not**
an error: the GC may legitimately track references the interpreter frame does
not resume from, e.g. a spilled temporary between two bytecodes.

---

## 4. What the install-time verifier proves

`DeoptVerifier` (in `jit/src/deopt.rs`) runs over the `DeoptimizationPoint`s an
artifact is about to install and returns `CompileResult<()>`. A failure is a
`Bailout`, i.e. the method falls back to the interpreter or the single-pass
backend — always semantically valid — instead of installing code whose deopt
cannot be reconstructed.

It collects every violation rather than short-circuiting (same rationale as
`ir_verify`: the second violation usually explains the first), never panics, and
never mutates.

| Lane | Active when | Proves |
| --- | --- | --- |
| scope | any `MethodFrameLimits` registered | every scope (including inlined caller scopes) names a known method, resumes at `bci < code_len`, and declares at most `max_locals` locals / `max_stack` stack slots |
| oop-map agreement | any `OopCoverage` registered, or `requiring_oop_map(true)` | every reference-typed slot in the deopt map is covered by a completeness-claiming oop map, and every reference offset is `i16`-encodable |
| removed-node | retired node ids registered | no slot names an IR node the optimizer removed unless that node also carries a materialization recipe |
| structural | always | point bci ≡ frame bci; each virtual-object id defined exactly once per scope; every `VirtualObjectRef` resolves; `num_fields` ≡ `field_values.len()`; register descriptors within the 16-entry GPR/XMM files the stub spills; no raw heap address baked into metadata; monitor list replayable (no depth-0 entry, no duplicate object, no unreconstructable monitor object); `deopt_points` sorted for `find_deopt_point`'s binary search |

It also makes the *runtime* resolver fallible: `try_resolve_value` returns a
structured `DeoptMetadataError` for an out-of-range register descriptor instead
of indexing `gpr[17]` and panicking inside the deopt trampoline, and
`resolve_value` degrades such a slot to `Unsupported` (refuse ⇒ safe re-run)
rather than to a plausible-looking wrong value.

Every error names the deopt point (native PC), the scope (depth + method key),
the bci and the slot — see `SlotRef`.

### Relationship to the VM-side resume verifier

The VM already has a *resume-time* checker behind `CRATONVM_DEOPT_VERIFY`
(`cratonvm_jit::deopt_verify_enabled`, consumed around
`vm/src/runtime/interpreter.rs:13885`): it re-checks slot counts against the
live frame's `max_locals`/`max_stack`, validates virtual-object descriptors, and
runs an oop-plausibility scan before writing anything into the frame.

The two are complementary and should not be merged:

* the VM one runs **per deopt**, on the reconstructed frame, and can only
  refuse (fall back to re-run) — by then the artifact is installed and running;
* this one runs **once per compile**, on the emitted metadata, and can refuse
  to install at all. It can also check things the VM cannot see: agreement with
  the oop map, agreement with the set of nodes the optimizer removed, and the
  sortedness `find_deopt_point` depends on.

They should share an invariant list so a rule added to one is added to the
other. Today they overlap only on the slot-count check.

### What it does **not** prove

* That the metadata is *complete*: it checks agreement and well-formedness, not
  that every live value has a slot. A frame that simply forgot a local passes.
* That `Undefined` is honest. A slot that *should* be
  `MaterializationRequired` but says `Undefined` is indistinguishable from a
  genuinely dead slot at this level — which is why §2's producer changes are the
  actual fix and this verifier is the backstop.
* Anything about inlined scopes that no producer builds (§1).

---

## 5. Remaining gaps, in priority order

1. **`apply_ea_to_ir` does not maintain safepoint snapshots**
   (`jit/src/lib.rs:6999-7035`, another agent's file). It sets `Op::Dead`
   directly for scalar-replaced loads, eliminated stores, the `Op::New` and
   elided locks, and rewires load consumers over `nodes` only — it never touches
   `graph.safepoints`. Required change: after the rewiring loop, walk
   `graph.safepoints` and, for every slot naming a node this pass killed, either
   (a) redirect it to the replacement node when the load had one (the
   `reverse_map` lookup already computed it), or (b) write a marker that says
   *eliminated*, not *undefined*. Today neither happens, so the slot survives
   naming an `Op::Dead` node until `ir_optimize::eliminate_dead_nodes` normalises
   it to `NO_NODE` (`jit/src/ir_optimize.rs:2024-2051`), which
   `ir_lower::frame_value_for` maps to `FrameValue::Undefined` — and a
   reference-typed local reconstructs as `Int(0)`/null. **A scalar-replaced
   object can currently reconstruct incorrectly for exactly this reason.**
2. **Inlined scope chains are never *populated*.** The IR lowerer can now build
   one (`caller_chain_for` + `InlineScopeTable`), and the verifier and
   `reconstruct_frame` have always walked one — but no producer pushes a scope,
   and the single-pass backend has no scope stack at all, so `FrameState::caller`
   is `None` in every artifact this VM installs. Until a producer lands,
   "byte-for-byte equivalent interpreter state" is still unreachable for any
   method the inliner touched. *Half-closed: the consumer side is done, the
   producer side is not.*
3. **The reexecute flag is recorded but not read.**
   `DeoptimizationPoint::semantics` exists and every producer stamps
   `ResumeSemantics::for_reason(reason)`, so the convention is written down in
   one place. The remaining gap is the consumer: the VM resume sink
   (`vm/src/runtime/interpreter.rs`) still re-derives the decision from
   `DeoptReason`, so a producer that knows better — a caller scope, a genuine
   post-call resume point — cannot yet change what the interpreter does.
4. **Monitor state is only recorded for elided locks** (1-pass) or not at all
   (IR). An ordinary open `synchronized` region at a deopt bci contributes no
   `MonitorInfo`, so a resume would not re-acquire it. Currently masked by
   `can_deopt_resume = false` for any method with an elided monitor
   (`jit/src/x64.rs:24180`) and by the VM sink's `is_synchronized` bail, but that
   is coverage loss, not correctness.
5. **Operand-stack widths in the 1-pass backend** are method-level
   (`uses_long_float_double`), so any method touching a `long`/FP records most
   non-oop stack slots `Unsupported` and can never resume precisely
   (`jit/src/x64.rs:2449-2465`).
6. **`ir_verify`'s frame-state lane is off by default**
   (`jit/src/ir_verify.rs:892-943`) because optimized graphs routinely carry
   snapshot slots pointing at removed nodes. Gap 1 is the reason; fixing it is
   what makes that lane affordable to enable.
7. **No producer populates `MaterializationRequired` yet** (§2). The variant and
   its semantics exist; the emitters still write `Undefined`.

## 6. Wiring the verifier (edits outside `jit/src/deopt.rs`)

**One of the two install sites is wired.**

* **DONE — `jit/src/ir_lower.rs`**, in `lower_inner_with_scopes`'s
  "Install-time deopt-metadata verification" block, before the artifact becomes
  a `CompiledMethod`. It builds a `DeoptVerifier` from `MethodFrameLimits` under
  the empty method key the lowerer records, the `Op::Dead` node ids as removed
  nodes, the scalar-replacement map's keys as materializable nodes, and one
  `OopCoverage` per **anchored** `OopMapEntry`. Entries with
  `native_pc_offset == 0` are deliberately not registered — `emit_safepoint_map`
  matches them by safepoint id, not pc, so registering them all under key `0`
  would compare deopt points against an arbitrary map. Both `deopt_points` and
  the baked `deopt_boxes` are checked.
* **STILL OPEN — `jit/src/x64.rs`**, before
  `cm.can_deopt_resume = !cm.deopt_points.is_empty() && !compiler.has_elided_monitor;`.
  The single-pass backend installs its deopt metadata unverified. The edit is to
  run the same check and **clear `can_deopt_resume`** rather than fail the
  compile, since this is the fallback tier and must always produce code.

`BailoutReason::DeoptMetadata(String)` **exists** (`jit/src/bailout.rs:131`,
category `"deopt_metadata"` at `:154`, rendered at `:190`, and covered by the
`all_reasons()` tripwire at `:376`), and `deopt_metadata_bailout`
(`jit/src/deopt.rs:4046`) uses it with the context string **`phase=install`**.
The earlier claim in this section — that it reuses `BailoutReason::IrVerification`
with `phase=deopt-metadata` — was wrong on both halves and has been removed.
