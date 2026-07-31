# Deoptimization metadata: what is emitted, what is proved, what is missing

Scope: the P0 items *"Complete deoptimization metadata"* and *"Emit precise oop
maps at every safepoint"* of `docs/known-issues/deep-research-vm-c2.md`.

Acceptance criteria under audit:

> Any guard or dependency failure reconstructs byte-for-byte equivalent
> interpreter state.

> Moving GC at every call, allocation, poll, and deopt site preserves all
> objects and updates every reference.

Neither is met today. This document records exactly which pieces exist, what
`jit/src/deopt.rs` now proves before an artifact is installed, and the ordered
list of what is still missing.

---

## 1. Completeness table

Two backends emit deopt metadata and they are at different stages, so each gets
a column. "IR" is the optimizing sea-of-nodes tier (`jit/src/ir_lower.rs`);
"1-pass" is the baseline single-pass backend (`jit/src/x64.rs`).

| Metadata element | IR | 1-pass | Where | Test |
| --- | --- | --- | --- | --- |
| Native PC → deopt point | **emitted** | **emitted** | `jit/src/ir_lower.rs:3942-3964` (`build_deopt_points`, keyed by `bci_native`); `jit/src/x64.rs:2315-2585` (`build_and_record_deopt_point`, keyed by `buf.pos()`) | `deopt.rs::unsorted_points_break_the_binary_search_and_are_rejected` |
| Inlined scope chain | **absent** | **absent** | `FrameState::caller` is hard-coded `None` at `jit/src/ir_lower.rs:3459,3783,3792` and `jit/src/x64.rs:2577`. The *type* supports arbitrary depth (`FrameState::caller`, walked by `reconstruct_frame`) and the inliner does inline — so every inlined callee's frame is attributed to the caller's method key with the callee's bci | `deopt.rs::inlined_caller_scopes_are_checked_too` (proves the verifier walks the chain, not that a producer builds one) |
| BCI | **emitted** | **emitted** | `DeoptimizationPoint::bci` + `FrameState::bci`, both set from the snapshot bci | `deopt.rs::bci_past_the_end_of_the_method_is_rejected`, `point_bci_must_match_its_frame_state_bci` |
| Locals | **emitted, typed from IR node type** | **emitted, typed from a whole-method classifier** | IR: `jit/src/ir_lower.rs:3369-3444` (`frame_value_for` / `typed_stack_slot`, driven by `IrType`); 1-pass: `jit/src/x64.rs:2347-2447` (oop mask ∪ `local_kinds`, with a per-bci refinement for `Ambiguous`) | `deopt.rs::reconstruct_resolves_typed_slots`, `more_locals_than_max_locals_is_rejected` |
| Operand stack | **emitted** | **approximated** | IR: same mapper as locals. 1-pass: `jit/src/x64.rs:2449-2525` — the abstract operand stack has **no per-entry width source**, so a non-oop slot in a method that touches any `long`/`float`/`double` is recorded `Unsupported` (refuse) unless an `invokedynamic` descriptor types it | `deopt.rs::deeper_stack_than_max_stack_is_rejected` |
| Locks / monitor state | **absent** | **partial** | IR: `monitors: Vec::new()` unconditionally (`jit/src/ir_lower.rs:3458,3782,3791`). 1-pass: only monitors on **scalar-replaced** objects whose lock was elided (`jit/src/x64.rs:2537-2545`, from `sr_monitor_at`); an ordinary `synchronized` block open at a deopt bci contributes nothing, and `can_deopt_resume` is switched off wholesale when any monitor was elided (`jit/src/x64.rs:24180`) | `deopt.rs::unbalanced_lock_state_is_rejected` |
| Constants | **emitted** | **emitted** | `FrameValue::Int/Long/Float/Double` from `Op::Const`/`Op::ConstF` (`jit/src/ir_lower.rs:3379-3395`), cat-1/cat-2 and int/FP distinguished | `deopt.rs::frame_value_int`, `reconstruct_resolves_fp_slots_and_xmm_registers` |
| Register locations | **unused by design** | **emitted** | `FrameValue::Register/RegisterLong/RegisterRef/XmmFloat/XmmDouble`, resolved against the stub-spilled `SavedRegisters`. The IR lowerer spills every value, so it never emits one | `deopt.rs::register_homed_locals_ignore_a_stale_canonical_frame_slot`, `out_of_range_register_descriptors_are_rejected` |
| Stack-slot locations | **emitted** | **emitted** | `FrameValue::StackSlot{,Ref,Long,Float,Double}(off)`, read as `*(rbp + off)` with `off < 0` | `deopt.rs::reconstruct_resolves_typed_slots` |
| Virtual (scalar-replaced) objects | **gated, partial** | **gated, partial** | IR: `jit/src/ir_lower.rs:3826-3936` (`frame_value_for_object`), only when `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`; bails to `Undefined` on nested virtuals, unproven dominance, or any unresolvable field. 1-pass: `jit/src/x64.rs:2302-2313` (`sr_virtual_object_state`) | `deopt.rs::virtual_object_graph_integrity_is_checked`, `slot_naming_a_removed_node_is_rejected` |
| Reexecute flag | **absent** | **absent** | No field exists. Re-execute-vs-resume is encoded *implicitly* in `DeoptReason`: div/rem and array guards document "the interpreter re-executes this bytecode" (`jit/src/ir_lower.rs:3464-3471`, `:2870`, `:2923`), while `PendingException` documents the opposite ("its `bci` names the throwing instruction … not a resume point", `deopt.rs`). A consumer that gets the convention wrong executes the instruction after a call that never returned | — (no test can pin an absent field) |
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
   info.class_id, cause))` with the cause the bail already knows
   (`DominanceUnproven` → `ScalarReplacedObject`, the nested-virtual bail →
   `NestedVirtualObject`, the field bail → `ScalarReplacedObject`).
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
2. **Inlined scope chains are never built.** `FrameState::caller` is always
   `None` in both backends, so a deopt inside an inlined callee cannot rebuild
   the caller frames. Until this lands, "byte-for-byte equivalent interpreter
   state" is unreachable for any method the inliner touched.
3. **No reexecute flag.** The re-execute-vs-resume decision is carried by
   convention through `DeoptReason` and prose. It needs to be a field on
   `DeoptimizationPoint` that the resume sink reads, not a per-reason
   convention each new consumer must re-learn.
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

The verifier is additive and currently has no caller. Two install sites should
run it:

* `jit/src/ir_lower.rs:4630` — after `cm.deopt_points = deopt_points;` and
  `cm.oop_maps = oop_maps;` (line 4655), build a `DeoptVerifier` from the
  method's `MethodFrameLimits`, one `OopCoverage` per `OopMapEntry` (mapping
  `native_pc_offset` → offsets and `moving_young_coverage_complete`), the
  scalar-replacement map's keys as materializable nodes, and the `Op::Dead` node
  ids as removed nodes; on `Err`, `record_bailout(&b)` and return `None` (the
  same `refuse(..)` shape already used at `jit/src/ir_lower.rs:4621-4626`).
* `jit/src/x64.rs:24180` — before `cm.can_deopt_resume = …`, run the same check
  and clear `can_deopt_resume` (rather than failing the compile) on `Err`, since
  the single-pass backend is the fallback tier and must always produce code.

A dedicated `BailoutReason::DeoptMetadata(String)` with category
`"deopt_metadata"` should be added to `jit/src/bailout.rs` (`BailoutReason`
around line 88, `CATEGORIES` at line 256, and the `all_reasons()` tripwire at
line 331). Until then `deopt_metadata_bailout` reuses
`BailoutReason::IrVerification` with a `phase=deopt-metadata` context.
