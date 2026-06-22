# Deopt-OSR Handoff: OSR-Exit (Steps 7-9) + Materializer Resume Wiring

**Status.** Two substrates are landed and committed on `feat/deopt-osr-scaffolding` (tip `bfd4f646`, worktree `C:/craton/CratonVM-deopt`), both gate-off byte-identical: (1) the **Steps 5-6 virtual-object materializer** — `materialize_virtual_objects` in `vm/src/runtime/deopt_materialize.rs`, two-phase, cycle-safe, with the `TempRootScope` RAII pin helper, plus the jit-model additions `VirtualObjectState::id` and `FrameValue::VirtualObjectRef`; and (2) the **x64 deopt-EXIT Steps 1-4** — the single-pass x64 backend snapshots a precise `FrameState` at the speculative-BCE loop-header guard (`emit_deopt_snapshot_at_guard`, `jit/src/x64.rs:6695`), spills 16 GPRs, calls `x64_deopt_entry` to reconstruct a `ReconstructedFrame`, and the interpreter sink (`vm/src/runtime/interpreter.rs`) RESUMES at the trapping bci via `resume_real_ir_deopt`, all gated `CRATONVM_DEOPT_REAL`. This doc covers the two remaining workstreams: **(A)** wire `materialize_virtual_objects` into the resume path so a deopt frame carrying virtual-object slots is re-materialized and resumed instead of bailing to re-run (smaller, higher-leverage — do first); **(B)** OSR-exit (Steps 7-9) — real-frame deopt at a loop bci, reusing the landed deopt-exit trampoline.

---

## Background: the substrate already in the tree

This section lists the reusable pieces with their file:line so a fresh engineer can find them cold.

### Deopt-EXIT infrastructure (Steps 1-4, landed)

- **Snapshot at guard** — `emit_deopt_snapshot_at_guard` (`jit/src/x64.rs:6695`) builds a `FrameState` from regalloc provenance (`Register`/`StackSlot`/`StackSlotRef` via `frame_value_for_slot`), using positive oop sources (`local_oop_masks` / `local_oop_reached` / `stack_oop_marks`). Called at the speculative BCE loop-header guard (`jit/src/x64.rs:14295`). EMIT-AND-DISCARD until Step 4 wired the resume.
- **Deopt-point storage** — `Compiler.deopt_points: Vec<DeoptimizationPoint>` (`jit/src/x64.rs:6181`), stable boxed copies `Compiler.deopt_boxes` (`jit/src/x64.rs:6184`) so the deopt-exit stub can bake a box pointer as arg0, and `Compiler.deopt_box_ptr_by_bci` (`jit/src/x64.rs:6195`).
- **Trampoline + reconstruct** — x64 stub `x64_deopt_entry` (jit `deopt.rs`) takes the 16 spilled GPRs, reconstructs and stashes a `ReconstructedFrame`.
- **Interpreter sink** — `build_deopt_frame_inner` (`vm/src/runtime/interpreter.rs:7630`) GC-roots reconstructed oops into `native_pin_roots` before the pool refill, then re-reads each oop from its forwarded pin slot after refill (moving-GC correct, `interpreter.rs:7668-7692`); `resume_real_ir_deopt` (`interpreter.rs:7745`) builds the frame, calls `push_frame_and_fire_entry` (`interpreter.rs:7065`), and returns `FramePushed` instead of re-running. Gated at the call site (`interpreter.rs:19702`).
- **Mapper that currently bails** — `ir_deopt_frame_values_with_objects` (`vm/src/runtime/interpreter.rs:7587-7610`): handles `Int`, `Undefined`, `Object(addr)`; catch-all `_ => None` at **`interpreter.rs:7607`** bails on `VirtualObject`, `VirtualObjectRef`, `Register`, `StackSlot`, `StackSlotRef`, `Float`, cat-2, `Unsupported`. This `None` propagates `build_deopt_frame_inner → None → resume_real_ir_deopt → None`, and the sink falls through to re-run.

### The materializer (Steps 5-6, landed)

- **Entry point** — `materialize_virtual_objects(shared, thread, frame: &mut ReconstructedFrame, stress_gc) -> Result<Vec<(usize, u64)>, MethodCallFailed>` (`vm/src/runtime/deopt_materialize.rs:139`). Mutates the frame in-place: rewrites every `VirtualObject`/`VirtualObjectRef` slot to `FrameValue::Object(addr)`; returns the (flat-slot, heap-addr) pairs.
- **Two-phase, cycle-safe** — Phase 1 (`deopt_materialize.rs:156-172`) collects every distinct `VirtualObjectState::id` (recursing through field graphs; `VirtualObjectRef(id)` terminates cycles) and allocates one pinned shell per id via `TempRootScope::alloc_shell`. Phase 2 (`deopt_materialize.rs:181-188`) fills fields via `field_value_to_value` + `store_field_barriered` (the real `putfield` SATB/card barrier). Frame rewrite at `deopt_materialize.rs:192-209`.
- **Field resolver fallback** — `field_value_to_value` (`deopt_materialize.rs:242-263`) maps `Int`/`Object`/`Undefined`/nested-virtual to a `Value`, but returns `Err(InternalError)` for `Float`/`Long`/`Double`/`StackSlot`/`StackSlotRef`/`Register`/`Unsupported` — so a cat-2 or FP field cleanly aborts to re-run.
- **GC-root RAII** — `TempRootScope` (`deopt_materialize.rs:44-105`): saves `base = thread.native_pin_roots.len()`, `alloc_shell` pushes each new shell to `native_pin_roots` **immediately** after `alloc_object_shared` (so a subsequent allocation can't reclaim it), re-reads shell addresses from pins after any GC, and `Drop` truncates back to `base`. **Caveat:** as landed, `Drop` runs at the end of `materialize_virtual_objects` — see Workstream A for why that is an unrooted window that must be fixed before live wiring.
- **Model additions** — `FrameValue::VirtualObjectRef(usize)` (`jit/src/deopt.rs:108`) and `VirtualObjectState::id` (`jit/src/deopt.rs:126`).
- **Acceptance tests already present** — `shells_materialize_and_survive_forced_gc`, `materializes_primitive_and_object_fields`, `materializes_two_object_cycle` (in `deopt_materialize.rs`).

### Scaffolding fields and gates (landed)

- `CompiledMethod.can_deopt_resume: bool`, `can_osr_exit: bool`, `compilation_epoch: u64` — all default `false`/`false`/`0` (`jit/src/lib.rs:1058-1070`; defaults at `1171-1173`, `1221-1223`).
- `deopt_real_enabled()` (`jit/src/lib.rs:859`, `CRATONVM_DEOPT_REAL`, read-once) and `deopt_verify_enabled()` (`jit/src/lib.rs:869`, `CRATONVM_DEOPT_VERIFY`).
- `DeoptReason::OsrExit` (`jit/src/deopt.rs:52-58`) — countable separately in the log, currently routed through the count-based default like `UncommonTrap`.
- `DeoptimizationLog::recommend_action` (`jit/src/deopt.rs:287-330`) — full HotSpot escalation already implemented: first → `Reinterpret`; `2..threshold/2` → `RecompileAndReinterpret`; `threshold/2..threshold` → `MakeNotEntrant`; `>= threshold` → `MakeNotCompilable`.

### OSR-ENTRY machinery (to reuse for OSR-exit PC selection)

- **Canonical loop-boundary PC set** — `osr_entry_native: Vec<i32>` (`jit/src/x64.rs:5719`, alloc `x64.rs:13732`), set positive at a boundary PC (`x64.rs:13971`) or `-1` when the PC is inside a LICM-hoisted loop body (`x64.rs:13960-13969`). A PC with `osr_entry_native[pc] >= 0` is at a loop boundary, outside all hoisted bodies — exactly the PC set OSR-exit needs.
- **Loop detection** — `loop_analysis::detect_loops` (`jit/src/loop_analysis.rs:90`); hoist analysis `find_loop_hoists` (`x64.rs:4320`), `find_arith_loop_hoists` (`x64.rs:4746`).
- **Entry predicate / function** — `CompiledMethod::can_osr_enter` (`jit/src/lib.rs:1525`), `osr_enter` (`jit/src/lib.rs:1543`); transfer of `osr_entry_native → osr_pc_to_native` at `x64.rs:22074`.
- **Trigger/policy** — `osr_threshold = 10_000` (`vm/src/runtime/jit_integration.rs:58`), backoff `should_try_osr` (`vm/src/runtime/frame.rs:518-536`), `OSR_MAX_ATTEMPTS = 5` (`frame.rs:506`), driver `try_osr` (`vm/src/runtime/interpreter.rs:16483`), orchestration `try_osr_with_backoff` (`interpreter.rs:4285-4320`).
- **Shadow-stack OSR-frame tracking** — gate `CRATONVM_SHADOW_OSR_TRACK` (`osr_shadow_track_enabled`, `jit/src/lib.rs:1622-1625`), default OFF because tracking the OSR'd Binary Trees frame regresses bt18 (conservative-pin × precise-move interaction). OSR-exit should inherit this gate (default OFF).

---

## Workstream A — Wire the materializer into the resume path

**Do this first.** It is smaller, self-contained, and unblocks the most common deopt case (scalar-replaced objects) on the already-landed deopt-exit path.

### Integration point

Today the resume mapper bails on virtual slots at **`vm/src/runtime/interpreter.rs:7607`** (the `_ => None` arm of `ir_deopt_frame_values_with_objects`, `interpreter.rs:7587-7610`). The mapper is invoked from `build_deopt_frame_inner` at **`interpreter.rs:7642-7643`**:

```rust
let locals = ir_deopt_frame_values_with_objects(&rframe.locals)?;
let stack_vals = ir_deopt_frame_values_with_objects(&rframe.stack)?;
```

### Design: materialize → rewrite → build+push

Insert, before the two mapper calls in `build_deopt_frame_inner`:

1. **Detect** whether the frame carries virtuals:
   ```rust
   let has_virtual = rframe.locals.iter().chain(&rframe.stack)
       .any(|v| matches!(v, FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_)));
   ```
2. **Materialize** on a mutable copy when present. `materialize_virtual_objects` takes `&mut ReconstructedFrame` (not the immutable `FrameState` baked in compiled metadata) precisely because it rewrites slots in place. On success the copy's `VirtualObject`/`VirtualObjectRef` slots are now `FrameValue::Object(addr)`; on `Err` (unsupported field, unknown id) return `None` to fall back to re-run.
3. **Map** the materialized copy with the existing `ir_deopt_frame_values_with_objects` — every slot is now `Object`/`Int`/`Undefined`, so it no longer hits the `_ => None` arm.

### GC-rooting handoff (load-bearing — mirror the Step-4 invariant)

The Step-4 sink holds pins across the frame build and push: `build_deopt_frame_inner` pins reconstructed oops into `native_pin_roots` before the pool refill (`interpreter.rs:7648-7652`), holds them across `push_frame_and_fire_entry` (`interpreter.rs:7065`, which may GC firing entry hooks), and truncates only after the frame is on `thread.frames` (`interpreter.rs:7758`). The materialized shells must obey the same invariant: **rooted continuously from allocation until the resumed frame is on `thread.frames`.**

The landed `TempRootScope` Drop runs at the end of `materialize_virtual_objects` (`deopt_materialize.rs:103`), which truncates the shell pins **before** the caller builds and pushes the frame — an unrooted window across a GC-capable build. **Required fix:** return the scope to the caller so it outlives the push:

```rust
pub(crate) fn materialize_virtual_objects(
    shared, thread, frame, stress_gc,
) -> Result<(Vec<(usize, u64)>, TempRootScope), MethodCallFailed>
```

Caller in `build_deopt_frame_inner`:
```rust
let (_addrs, _temp_scope) = materialize_virtual_objects(shared, thread, &mut frame_copy, false)?;
// map → pin reconstructed oops → refill → build frame → push_frame_and_fire_entry
drop(_temp_scope); // release shell pins ONLY after the frame is on thread.frames
```

The shells are then rooted by the pins, then by both the pins and the frame, then by the frame alone — no unrooted moment. The existing per-oop re-pin loop (`interpreter.rs:7645-7652`) already re-pins the `Object(addr)` slots that materialization produced; that is redundant but harmless, or the loop can skip slots already held by `_temp_scope` once the watermark is threaded through.

### Elided-monitor caveat

A scalar-replaced object may have had its monitor elided during JIT lock elision. A resumed frame that later runs `monitorexit` on such an object would hit an un-entered monitor. **Gate:** keep `can_deopt_resume = false` (i.e. block live resume) when the compilation's `scalar_replaced` set is non-empty *or* the method is `ACC_SYNCHRONIZED`, per the Phase-A gate in `deopt-osr.md` (~lines 454-457). Re-entering elided monitors at resume is a later refinement, out of scope here.

### Cat-2 interaction

Materialization is orthogonal to cat-2 support. A `long`/`double` field of a virtual object resolves through `field_value_to_value`'s `Err` arm (`deopt_materialize.rs:253-260`) → re-run. Cat-2 at the **top level** of locals/stack still bails at `interpreter.rs:7607` (the `SavedRegisters` snapshot is `gpr[16]` only; FP/cat-2 needs the `xmm[16]` extension). Both fall back safely; neither blocks this workstream for the integer/object-graph case.

### Increments (independently landable) + test plan

**A1–A3 — DONE (2026-06-21, branch `feat/deopt-osr-completion`).** The materializer
is wired into `build_deopt_frame_inner`: a reconstructed frame carrying
`VirtualObject`/`VirtualObjectRef` slots is materialized into a real heap object
graph and resumed at the trapping bci instead of bailing to re-run.

- **A1 — DONE (keep_pins, not a returned scope).** The literal "return a
  `TempRootScope`" plan does not compose — `TempRootScope` holds `&mut JvmThread`,
  so returning it would freeze the thread for the rest of the GC-capable build.
  Instead `materialize_virtual_objects` gained a `keep_pins: bool`: on the live
  path (`true`) it `mem::forget`s the scope so the shell pins persist in
  `native_pin_roots`; the sink owns release via its existing `pin_base..truncate`
  window held across `push_frame_and_fire_entry` — continuous rooting, no unrooted
  window. Error path always releases (scope drops before the early return).
  `ReconstructedFrame` derives `Clone` so the sink materializes on a copy.
- **A2 — DONE.** `has_virtual` branch in `build_deopt_frame_inner` clones →
  materializes (keep_pins) → maps. Elided-monitor gate bails (re-run) for
  `ACC_SYNCHRONIZED`; the residual synchronized-*block* lock-elision case is
  undetectable from the frame and unreachable today (no production emitter writes
  `VirtualObject` deopt slots), documented as a flag the future x64 virtual-slot
  emitter must carry. The coverage gate `can_deopt_resume` is also now finalized
  (x64-backport Step 5, below) and consulted at the sink.
- **A3 — DONE.** 4 vm-lib tests: `resumes_frame_with_virtual_object` (+forced-GC
  survival via the pushed frame), `resumes_frame_with_object_cycle`,
  `virtual_resume_blocked_for_synchronized_method`,
  `virtual_shells_survive_forced_gc_during_build`. 31/31 deopt lib tests green;
  gate-off byte-identical.
- **A4 / verifier increment 1 — DONE (2026-06-21, branch `feat/deopt-osr-verifier`).**
  `deopt_verify_enabled()` now has a consumer: `verify_reconstructed_frame`
  (pure, unit-tested) runs in `build_deopt_frame_inner` under `CRATONVM_DEOPT_VERIFY`
  and checks the always-sound structural invariants a map drift most often
  breaks — slot counts past `max_locals`/`max_stack` (a shifted snapshot), a
  malformed `VirtualObject` descriptor (declared `num_fields` ≠ `field_values`),
  and a dangling `VirtualObjectRef`. A violation is reported to stderr and forces
  the safe re-run (fail-safe). It deliberately does NOT type-check per-slot
  (cat-2/FP `Unsupported` slots legitimately re-run — flagging them would be a
  false positive). 4 tests; 35/35 deopt lib tests green; gate-off byte-identical.
- **A4 / verifier increment 2 — DONE (2026-06-21, branch `feat/deopt-osr-verifier`).**
  Oop-plausibility layer `verify_reconstructed_oops`: every `Object(addr)` slot
  (locals, stack, and recursively the already-real `Object` fields of
  scalar-replaced descriptors) must be null or a real heap address
  (`heap.is_heap_addr` — alignment + region containment, no header deref, safe on
  a garbage word). Catches the most dangerous drift the structural layer cannot —
  a non-oop value (small int / wild pointer) in a slot resumed as an object ref, a
  UAF on first deref. Fail-safe (forces re-run). 1 test; 36/36 deopt lib green;
  gate-off byte-identical. **The verifier's runtime fail-safe role is now complete
  (structural + oop layers).**
- **A4 / verifier increment 3 — the eager-deopt VALUE differential — INFRA-BLOCKED
  (= task #7).** Force a deopt at a *non-failing* guard, reconstruct, and compare
  the reconstructed-interpreter end-result against the JIT result — the form that
  catches a *value* drift (right slot count + valid oops, wrong contents) the two
  runtime layers cannot. Two concrete blockers, the same ones that block the
  end-to-end BCE test: (1) the `deopt_real_enabled()` / `deopt_verify_enabled()`
  gates are **read-once cached**, so one in-process test cannot run the method both
  gate-on and gate-off to diff — it needs a *separate-process* harness (spawn the
  VM twice with different env, diff output / golden checksum) **or** a `pub`
  non-`cfg(test)` gate override; (2) forcing a deopt at a guard the speculation
  did NOT fail needs a codegen trigger (an unconditional deopt at the BCE guard,
  analogous to the existing `osr_exit_test_trigger_bci` / `CRATONVM_OSR_EXIT_TEST`).
  Recommended path: add the forced-BCE-deopt gate in `emit_deopt_stubs`, then a
  scripted separate-process differential over a golden-checksum benchmark (bt18 =
  68332206) under `CRATONVM_DEOPT_REAL=1` + the force gate.

**x64-backport Step 5 — `can_deopt_resume` coverage gate DONE (2026-06-21).** At
x64 finalize `cm.can_deopt_resume = !deopt_points.is_empty() &&
scalar_replaced.is_empty()` (mirrors `can_osr_exit`). The `scalar_replaced`
clause is load-bearing: this backend records a scalar-replaced slot by machine
provenance (Register/StackSlot), not as a `VirtualObject`, so its snapshot can't
be re-materialized and lock elision over it makes mid-method resume unsound — such
methods stay on re-run until the x64 emitter writes `VirtualObject` slots + an
elided-monitor flag. The sink (`execute_jit_call` already holds the
`CompiledMethod`) attempts `resume_real_ir_deopt` only when
`compiled.can_deopt_resume`. Empty/false unless `deopt_real_enabled()`, so
production artifacts unchanged; 825 jit-lib tests green.

---

## Workstream B — OSR-exit (deopt-osr Steps 7-9)

OSR-exit = a real-frame deopt taken at a **loop bci** (not a guard bci): a running OSR-compiled loop bails mid-loop back to the interpreter and resumes the loop body. It reuses the landed deopt-exit Steps 1-4 trampoline + reconstruct + resume; the only genuinely new machinery is emitting exit maps at the OSR-vetted loop-boundary PC set and routing the mid-loop branch through the trampoline.

### Step 7 — emit exit maps at loop boundaries (additive, emit-and-discard)

- **New `emit_osr_exit_map_at(loop_bci)`** on the x64 `Compiler` — a near-clone of `emit_deopt_snapshot_at_guard` (`jit/src/x64.rs:6695`): build a `FrameState` from regalloc provenance and oop masks, but tag the `DeoptimizationPoint` with `DeoptReason::OsrExit` (`jit/src/deopt.rs:52`). Record into `deopt_points`/`deopt_boxes` like the guard path, and additionally into a **new `Compiler.osr_exit_points: Vec<usize>`** mirroring `deopt_points`.
- **PC set** — call `emit_osr_exit_map_at(pc)` exactly at the boundary PCs already vetted by OSR-entry, i.e. where `osr_entry_native[pc] >= 0` (`jit/src/x64.rs:13971`), right after the existing OSR-entry soundness check (`x64.rs:13960-13969`). This inherits the LICM-hoist rejection for free — no PC inside a hoisted body becomes an exit point.
- **`can_osr_exit`** — at finalize, set `cm.can_osr_exit = !osr_exit_points.is_empty()` (or conservatively keep `false` until validated), mirroring `can_deopt_resume`. Transfer `osr_exit_points` to a new `CompiledMethod.osr_exit_points` field.
- **Lands:** unit test asserting an exit map exists at a known loop header with locals matching the regalloc model. Pure additive; no exit path is taken yet. Risk: low.

### Step 8 — flip OSR-exit (route mid-loop bail through the trampoline)

- At the interpreter deopt sink (near the Step-4 call site, `vm/src/runtime/interpreter.rs:19702`), add a loop-bci branch: when `cm.can_osr_exit && deopt_real_enabled()` and the trapping bci is in `osr_exit_points`, route through the **same** deopt trampoline → `x64_deopt_entry` → `build_deopt_frame_inner`/`resume_real_ir_deopt`, reconstructing at the **loop bci** and pushing an interpreter frame that resumes the loop body — not from method entry.
- Add an `is_osr_exit_bci(bci)` predicate (lookup in `CompiledMethod.osr_exit_points`, or cache the loop-header set from `detect_loops`).
- **Disable the `i64::MIN` re-run sentinel for OSR-compiled methods once `can_osr_exit` holds** — an OSR-exit must resume the loop body, never re-run the method from entry (re-running an OSR'd method that started mid-loop is semantically wrong). Scope the sentinel-disable narrowly to `compiled_via_osr && can_osr_exit`.
- The first trigger to wire is a deliberate **uncommon-trap branch inside an OSR'd loop** (instrument a rare branch). **Lands:** an OSR'd loop taking the rare branch resumes interpreting the loop body at the correct bci with correct locals/stack. Gate: `CRATONVM_DEOPT_REAL` (shared). Risk: high.

### Step 9 — de-speculation wiring + epoch invalidation

- Route every real deopt/OSR-exit through `record_deopt` + `DeoptimizationLog::recommend_action` (`jit/src/deopt.rs:287-330`, already implemented). `OsrExit` events are countable separately via the new reason variant.
- **Implement `MakeNotEntrant`/`MakeNotCompilable`** by advancing `CompiledMethod.compilation_epoch` (`jit/src/lib.rs:1070`, field landed but no consumer): on invalidation, bump the epoch; before the resume path follows a boxed `DeoptimizationPoint` pointer, assert the owning method's current epoch matches the box's creation epoch, so stale boxes from a superseded compilation are never dereferenced.
- **Lands:** a method that deopts past threshold is made not-entrant; the next call re-enters cleanly with no use of a freed box; `DeoptimizationLog` reports the deopt rate. Risk: med.

### Scaffolding status for B

Already in the tree: `DeoptReason::OsrExit` (`deopt.rs:52`), `CompiledMethod.can_osr_exit` (`lib.rs:1064`), `compilation_epoch` (`lib.rs:1070`), `recommend_action` (`deopt.rs:287-330`), and the whole reusable PC-selection (`osr_entry_native`) + trampoline + sink.

**Step 7 — DONE (2026-06-21).** `emit_osr_exit_map_at` (`jit/src/x64.rs`, sharing `build_and_record_deopt_point` with the BCE-guard snapshot, tagged `DeoptReason::OsrExit`) is called at every OSR-vetted loop-boundary PC (`osr_entry_native[pc] >= 0`, right after the LICM-hoist check), **gated on `deopt_real_enabled()`** so production builds zero OSR-exit metadata and stay byte-identical. `Compiler.osr_exit_points` + `osr_exit_box_ptr_by_bci` (Step-8 keying) added; at finalize `cm.osr_exit_points = compiler.osr_exit_points` and `cm.can_osr_exit = !osr_exit_points.is_empty()` (`CompiledMethod.osr_exit_points` field added). Emit-and-discard — nothing consumes the maps yet. **Live-validated:** under `CRATONVM_DEOPT_REAL=1 CRATONVM_DBG_DEOPT=1`, a counted-loop method emits exit maps at each loop-body bci with `locals == max_locals` and stack depth tracking the operand stack; with the gate off, **zero** maps and identical result (`acc=14985000000`). 824 jit lib tests green.

**Step 8 — DONE (loop-bci resume + OSR-driver safety; 2026-06-21).** Two pieces landed, both gated:
- **Trigger codegen** (`jit/src/x64.rs`): a synthetic unconditional OSR-exit branch (reason 7) emitted on the **normal** loop path — right at `pc_to_native[pc]` (the back-edge/fall-through target, NOT the separate `osr_entry_native[pc]` landing pad — emitting there only caught OSR-*entries* and corrupted them) — at the first detected loop header (`osr_exit_test_trigger_bci`, the min `detect_loops` header). The `emit_deopt_stubs` frame-deopt branch was generalized to bake either the BCE-guard box (reason 2, `deopt_box_ptr_by_bci`) or the OSR-exit box (reason 7, `osr_exit_box_ptr_by_bci`) → `x64_deopt_entry` reconstructs at the loop bci → the **`execute_jit_call` sink (Step-4 `resume_real_ir_deopt`) resumes the loop body** at that bci. Gated on `CRATONVM_OSR_EXIT_TEST` + `CRATONVM_DEOPT_REAL`; `None` in production ⇒ no JMP ⇒ byte-identical.
- **OSR-driver safety** (`vm/src/runtime/interpreter.rs` `try_osr`, ~17590): a frame-deopt taken *inside OSR-entered code* stashes `LAST_DEOPT` and returns `i64::MIN`; OSR is same-frame replacement, so the driver must NOT treat the sentinel as the return value (`i64::MIN as i32 == 0` — the corrupt result this fixes). It now detects `result==i64::MIN && take_last_deopt().is_some()` and **rejects the OSR** (clears the stash, returns `None`) so the interpreter continues its own frame — safe because the trigger bails at the loop header before committing any JIT iteration. This is a general correctness fix for any frame-deopt in an OSR'd method.

**Live-validated** (`CRATONVM_DEOPT_REAL=1 CRATONVM_OSR_EXIT_TEST=1 CRATONVM_NO_IR_BRANCHY=1`): a counted-loop `sum(n)` — (A) n=10 (no OSR-entry): 150 OSR-exit deopts at the loop header, each reconstructs `[n,0,0]` and resumes → `acc` correct (9000); (B) n=1000 (OSR-enters): driver rejects the bail → no corruption → `acc` correct (99900000); gate-off controls identical. 825 jit lib tests green.

**Step 8 follow-up — TRUE OSR-exit transfer DONE (P4, 2026-06-21).** The *true*
OSR-exit now transfers JIT-advanced loop state back into the **live interpreter
frame** (overwrite locals + operand stack in place, set pc) instead of the safe
reject — closing the side-effect double-execution gap (reject discards the
JIT-advanced loop counter and re-runs committed iterations in the interpreter).
Both pieces gated, default-OFF ⇒ byte-identical production (bt18 = 68332206):

- **Transfer** (`vm/src/runtime/interpreter.rs` `transfer_osr_exit_into_live_frame`,
  gate `CRATONVM_OSR_EXIT_TRANSFER`): wired into `try_osr`'s OSR-exit branch as
  transfer-then-reject. It is an *in-place* mutation of `thread.frames[frame_idx]`
  (NOT a rebuild-and-swap), so the frame's identity/bookkeeping survives
  (`backward_count`, `osr_attempt_counts`, `monitor_on_exit`, `seq`, cold
  metadata). Reuses the `ir_deopt_frame_values_with_objects` mapper; maps BEFORE
  any write so a reject (cat-2/FP/virtual/out-of-scope) can never half-write the
  frame. GC-safe with NO pin dance: the OSR-exit snapshot carries only
  Register/StackSlot/StackSlotRef (never `VirtualObject`), so there is no
  materialization and no pool refill ⇒ no Java allocation between the in-stub oop
  capture and the in-place write; the frame slot roots the oop the instant it is
  written. Consulted with `compiled.can_osr_exit` + `deopt_real_enabled()`.
- **Realistic counter trigger** (`jit/src/x64.rs` `emit_osr_exit_after_trigger`,
  gate `CRATONVM_OSR_EXIT_AFTER=N`): bails on the N-th reach of the loop header so
  the JIT advances ~N iterations (and commits their side effects) before the exit —
  vs the unconditional-at-header `CRATONVM_OSR_EXIT_TEST` trigger that bails at
  iteration 0 (where reject and transfer coincide). Per-site leaked `Box<i64>`
  counter; RAX/RCX saved/restored via PUSH/POP (POP preserves EFLAGS, so the CMP
  result survives to the JL), RSP balanced before either exit ⇒ transparent to the
  JIT's machine state at the loop-header BB boundary.

**Live-validated** (`OsrXfer.loop(int)`: int counted loop with a per-iteration
static side effect `sink += 1`, n=200000) — `CRATONVM_DEOPT_REAL=1
CRATONVM_OSR_EXIT_AFTER=1000`: (A) `CRATONVM_OSR_EXIT_TRANSFER=1` → `sink=200000`
== HotSpot (10 transfers, reconstructed `locals=[200000, 1997001, 1999]` proving
genuinely advanced state); (B) transfer OFF (reject) → `sink=200999` (the ~999
committed-then-re-run iterations the transfer eliminates); n=50000 also ==HotSpot.
Gate-off bt18 = 68332206 byte-identical. 825 jit + 6 new vm-lib transfer tests
green. NOT default-on: still coupled to OSR-frame shadow-stack tracking
(`CRATONVM_SHADOW_OSR_TRACK`, default OFF, regresses bt18) for a production flip —
best co-scheduled with the moving-GC work; the mechanism + realistic trigger are
proven under the gate.

**Step 9 — DONE (P3, 2026-06-21)** (epoch-invalidation consumer for `compilation_epoch`
+ de-speculation wiring) — branch `feat/deopt-osr-epoch-invalidation`; see the
**Step 9 — DONE** record after the Workstream P2 section below.

---

## Workstream P2 — cat-2 / FP real-frame resume (branch `feat/deopt-osr-cat2fp`)

Real-frame deopt resume today supports only `int`/`ref` (+ `Undefined`) slots. A
`long`/`double`/`float` slot is mishandled. This workstream makes them resume.
Line numbers below are on `feat/deopt-osr-cat2fp` (off dev `e61be2ec`).

### The bug class (why this matters, not just coverage)

`frame_value_for_slot` (`jit/src/x64.rs:6311`) has **no per-slot width source**, so:
- a **`double`/`float`** local is XMM-resident → `frame_value_for_slot` returns
  `Unsupported` (`x64.rs:6321`) → the resume mapper bails → safe re-run. *Correct,
  just no resume.*
- a **`long`** local is GPR/spilled → recorded as `Register(r)`/`StackSlot(off)` →
  `resolve_value` (`jit/src/deopt.rs:840`) makes it `Int(i64)` → the resume mapper
  truncates to `i32`. **Silent corruption**, not a safe re-run.

### P2.0 — DONE (commit on this branch; merged to dev)

`can_deopt_resume` (`jit/src/x64.rs:22636-22639`) now also requires
`wide_local_high_halves(code, code_len).is_empty()`, so any method with a
long/double local takes the safe whole-method re-run. Closes the `long`
corruption; the BCE pilot (int loops) is unaffected. **P2.1 narrows this exclusion
to FP-only; P2.2 drops it.**

### P2.1 — long resume + P2.2 — float/double resume — ✅ DONE (2026-06-21, branch `feat/deopt-cat2-fp-resume`, off dev `0f21a83b`)

Both landed together (the substrate covers long AND FP) as 6 gated, gate-off
byte-identical increments. The implementation generalises the per-slot-`is_long`
sketch above into a single width/type oracle and is sound by a per-slot fallback,
not a coarse gate alone. **All gate-off byte-identical; 830 jit-lib + 37 vm-lib
deopt tests green.**

- **Inc 1 — model + resolver** (`jit/src/deopt.rs`). `SavedRegisters` gains
  `xmm:[u64;16]` (`#[repr(C)]`). New `FrameValue`: `Double(u64)`,
  `RegisterLong(u8)` (→ `Long(gpr[r])`), `XmmFloat(u8)`/`XmmDouble(u8)` (→
  `Float`/`Double` from `xmm[n]` — named for the XMM file, vs the sketch's
  `RegisterFloat`/`RegisterDouble`), `StackSlotFloat(i32)`/`StackSlotDouble(i32)`.
  `resolve_value` resolves all.
- **Inc 2 — stub XMM spill** (`jit/src/x64.rs`). The frame-deopt
  `SavedRegisters` region grows 128→256 B (gated) and the stub spills XMM0..15
  (`MOVQ`) after the GPRs; `&gpr[0]` is still the struct base.
- **Inc 3 — width source + typed emit** (`jit/src/x64.rs`). `classify_local_kinds`
  scans load/store/`iinc` opcodes → per-local `Int/Long/Float/Double/Ref/HighHalf`,
  `Ambiguous` on slot reuse, `Unknown` if never accessed (stored on
  `Compiler.local_kinds`, computed only under the gate). `typed_local_frame_value`
  maps a non-oop slot per kind+provenance; `HighHalf`/dead-`Unknown` → `Undefined`;
  `Ambiguous` / a provenance-kind contradiction → `Unsupported` (re-run, never a
  guess). The sketch's `is_long = hi.contains(&(i+1)) && xmm.is_none()` derivation
  is subsumed by the direct opcode classification (which also distinguishes
  float-vs-double, needed for P2.2).
- **Inc 4 — resume mappers** (`vm/.../interpreter.rs`, `deopt_materialize.rs`).
  As sketched: `fv_to_value` adds `Float`/`Double`; `ir_deopt_locals` collapses
  `Double` as well as `Long`; `build_deopt_frame_inner` swaps to
  `ir_deopt_locals` (locals, cat-2 collapse) + `ir_deopt_frame_values` (stack);
  the redundant `ir_deopt_frame_values_with_objects` is deleted (its test migrated).
  `field_value_to_value` maps Long/Float/Double virtual fields (completeness).
- **Inc 5 — gate relax** (`jit/src/x64.rs`). `can_deopt_resume` drops the wide /
  FP exclusion entirely (5a lifted long via a precise `has_fp_local` from
  `local_kinds`; 5b dropped FP) → now just `!deopt_points.is_empty() &&
  scalar_replaced.is_empty()`. **Soundness rests on the per-slot `Unsupported`
  net**, not the gate. Also hardened the oop path: a REGISTER-resident ref now
  emits `Unsupported` (it would otherwise resolve to `Int`, mistyping the slot and
  dropping the oop from GC tracking — a moving-GC UAF the wider gate could expose);
  spilled refs (`StackSlotRef`) are unaffected, and the BCE pilot spills its array
  ref so it is a no-op there.

**Trap recorded (`get_local` vs cat-2):** the frame's `CompactValue` is NaN-boxed
and cannot distinguish `long` from `double` by bits alone (it relies on the
parallel `local_kinds`); the generic `get_local`/`to_value` ignores that array, so
a `long` whose bits form a valid `double` reads back as `Double`. The frame stores
it correctly (raw bits + `LKIND_LONG`) and the resumed `lload` reads it as a long —
tests assert the raw 64-bit word (`get_local_raw`), not the NaN-boxed `Value` kind.

**Validation:** unit-complete (classifier / resolver / `typed_local_frame_value` /
`build_deopt_frame_inner` with long+float+double + cat-2 collapse alignment);
gate-off byte-identical **by construction** (deopt-real off ⇒ no snapshot emit ⇒
`deopt_points` empty ⇒ `can_deopt_resume` false, `local_kinds` empty, frame size
unchanged).

**✅ Through-JIT deopt validation — DONE (`CRATONVM_DEOPT_EAGER`).** The
"infra-blocked, no deterministic BCE-deopt trigger" gap is closed: under
`CRATONVM_DEOPT_EAGER` + `deopt_real`, the backend records a reason-2 snapshot at
the first loop header and emits an unconditional branch to the deopt stub at
`pc_to_native[pc]` (not coupled to the speculative-BCE guard, so it fires on any
loop). SEPARATE-PROCESS differential (the read-once-gate blocker): `NoArrayDeopt`
(long/double/float accumulator loops) under EAGER reconstructs `Long(499500)` /
`Double` / `Float` at the loop header (DBG_DEOPT trace) and under
EAGER+`OSR_EXIT_TRANSFER` USES it (60000 transfers) → result == HotSpot == gate-off
(a wrong cat-2/FP reconstruction would corrupt it). bt18=68332206 gate-off. The
cat-2/FP snapshot+XMM-spill+resolve+reconstruct+use chain is now proven end-to-end
through the live JIT.

**Remaining P2 follow-ups (genuinely deferred):** (a) typed operand-stack cat-2/FP
**RESUME** — FU2 made it SOUND (wide/FP methods re-run their non-oop stacks, never
mistype); resuming needs a per-bci operand-stack type-flow analysis (model each
opcode's stack effect) — low ROI for the gated OSR-exit path + simulator-bug risk,
so left as the sound conservative behavior. (b) register-resident *oop* typing —
DONE (Inc 6 `RegisterRef`), though the oop *mask* not marking a ref at a bci still
falls to re-run. (c) **production default-on flip** — blocked on moving GC: needs
precise OSR-frame tracking (`CRATONVM_SHADOW_OSR_TRACK`), which regresses bt18
(conservative-pin × precise-move); co-scheduled with the precise-JIT-maps work.

**Step 9 — DONE (de-speculation wiring + compilation-epoch staleness guard; 2026-06-21).** Branch `feat/deopt-osr-epoch-invalidation` off dev `3c9b628a`. The real-frame-deopt / OSR-exit resume sink (`execute_jit_call`, `vm/src/runtime/interpreter.rs`) now routes through a new `real_frame_deopt_resume_and_despeculate` helper:

- **De-speculation wiring** (the *"route every real deopt/OSR-exit through `record_deopt` + `recommend_action`"* item). Previously the real-frame-deopt sink recorded **nothing** — only the separate `jit_uncommon_trap` stub path fed the log. The sink now drives the EXISTING `DeoptimizationController::deoptimize` (`vm/src/jit/helpers.rs`) on every real-frame deopt: it records the event in `shared.deopt_log` (so the deopt rate is observable), evicts the artifact from `jit_cache` (= make-not-entrant: the next call recompiles), and on repeated deopts `recommend_action` escalates to `MakeNotCompilable` → `jit_skip_set` (stops recompiling). The reason is recovered from the `deopt_points` entry matching the trapping bci, so `OsrExit` events stay countable separately from guard deopts. De-spec runs AFTER the resume decision, so it only affects FUTURE invocations — the just-resumed frame is unaffected.
- **Epoch staleness guard** (the `compilation_epoch` consumer). New per-method live-epoch registry `SharedVm.method_epochs` (keyed `"<class>.<method>:<descriptor>"`, the deopt-log key). `deoptimize` advances it on each invalidation (`bump_compilation_epoch`); the **5** `jit_cache.put` install sites stamp the fresh artifact's `CompiledMethod.compilation_epoch` from the live epoch (`stamp_compilation_epoch`). Before resuming, the sink asserts `compiled.compilation_epoch >= live_epoch(M)`: a compilation **superseded** since it was installed (live advanced past its epoch) does NOT resume its now-invalidated speculation — it falls back to the safe whole-method re-run. **KEY:** a `DeoptimizationPoint` box is built during *compilation*, before install, so it cannot carry an install-time epoch; the box is reachable only through the artifact that baked it, so checking the artifact's `compilation_epoch` *is* "assert the owning method's current epoch matches the box's creation epoch before following it." NB: in default leak-mode the deopt boxes are never freed (`CompiledMethod::drop` `mem::forget`s `_deopt_point_boxes`), so this is a **correctness / de-spec** guard (never resume a superseded speculation), not a UAF guard; the literal in-stub before-deref check only matters under the `CRATONVM_JIT_FREE_CODE=1` A/B mode and is a documented follow-up.
- **Gate-off byte-identical.** Every stamp / bump / guard is gated on `deopt_real_enabled()` (OFF by default): the field stays `0`, the registry stays empty + unread, the sink helper is never reached (it lives inside `deopt_real_enabled() && can_deopt_resume`). bt18 golden checksum `68332206` unchanged with the gate off (== HotSpot). 18/18 `deopt_step3` vm-lib tests green, incl. **3 new** Step-9 tests (`step9_epoch_registry_bump_and_read`, `step9_fresh_artifact_resumes_and_records_deopt`, `step9_stale_artifact_skips_resume`).

**Step 9 follow-ups — ✅ ALL DONE (2026-06-22, branch `feat/deopt-osr-step9-followups` off dev `f911e67b`).** All three gated/inert in production; bt18 golden `68332206` verified byte-identical gate-OFF **and** gate-ON (== HotSpot). 835 jit-lib + 48 vm-lib deopt tests green (4 new).

- **(a) — in-entry before-deref epoch check (`DeoptEpochGuard`).** The literal "bake a stable live-epoch cell pointer alongside the box" is realised as a small, **process-lifetime-retained** `DeoptEpochGuard { creation_epoch: AtomicU64, live_epoch_cell: AtomicPtr<AtomicU64> }` (`jit/src/deopt.rs`) baked as a **4th arg** into every frame-deopt stub (`emit_deopt_stubs`, `R9`/`RCX`), and copied to `CompiledMethod::deopt_epoch_guard` at finalize. The VM stamps it at install (`stamp_compilation_epoch` → `stamp_deopt_epoch_guard`, gated `deopt_real_enabled()`) with the artifact's creation epoch and a STABLE pointer to the method's live-epoch cell — `method_epochs` is now `FxHashMap<String, Box<AtomicU64>>` so the cell address is process-stable (`SharedVm::live_epoch_cell_ptr`). `x64_deopt_entry` consults the guard FIRST — before touching `point`/`regs` — and on a superseded artifact stashes a sentinel re-run frame (`bci = u32::MAX`) without dereferencing the (possibly-freed) box. Done in `x64_deopt_entry` rather than hand-emitted machine code because it is equivalent and far less risky in the hot stub; the box read is genuinely avoided. ALSO: deopt-point boxes are now retained (leaked) EVEN under `CRATONVM_JIT_FREE_CODE=1`, decoupling box lifetime from code lifetime so the baked box pointer never dangles. **Live-validated:** under `CRATONVM_DEOPT_REAL=1 CRATONVM_DEOPT_EAGER=1` a counted array loop fires the SUPERSEDED short-circuit (`creation_epoch=0 < live`) and still produces the HotSpot result. Tests: `superseded_guard_skips_box_deref_and_stashes_rerun_sentinel`, `fresh_guard_proceeds_to_reconstruct`, `fua_stamp_deopt_epoch_guard_and_supersede`.

- **(b) — eager recompile re-queue.** `DeoptimizationController::deoptimize` (`vm/src/jit/helpers.rs`) now, on a `RecompileAndReinterpret` action, eagerly enqueues a high-priority `CompilationTask` via the tiered manager when a background compiler is active — instead of waiting for the method to re-cross the interpreter hotness threshold (up to `JIT_RETRY_STRIDE` calls). Gated on `deopt_real_enabled()` (production uncommon-trap path byte-identical) and on `compiler_active()` (no-op + no queue leak when no worker drains it; the hotness-retry path still recompiles, so behaviour never regresses).

- **(c) — per-bci de-spec (suppress just the failed speculation).** A process-global `(method_key, bci)` de-spec registry (`jit/src/deopt.rs`: `despec_insert`/`despec_contains`, empty + allocation-free fast-path in production). `DeoptimizationLog::deopt_count_at_bci` counts per-site. The resume sink (`real_frame_deopt_resume_and_despeculate`), after recording a deopt, inserts `(method, bci)` once a SINGLE site has deopted ≥ `PER_BCI_DESPEC_LIMIT` (4, HotSpot `PerBytecodeTrapLimit`-like) — so a method with one pathological speculation site de-specs THAT site and stays compilable instead of escalating to a whole-method blacklist (a method with diffuse deopts still hits the per-method backstop). The optimizing backend consults the registry (`method_key` plumbed through `compile_with_param_slots`) and drops any speculative-BCE loop-header guard recorded there, falling back to per-access bounds checks on recompile. Skipped for the superseded-sentinel bci (`u32::MAX`) so a stale artifact never de-specs a speculation that may not be current — correct by design. Test: `step9_fuc_per_bci_despec_after_limit` drives 4 deopts at one bci through the real sink and asserts the de-spec fires (and not before, and not for a different bci); `despec_registry_is_per_method_bci`.

Residual (genuinely deferred): a through-JIT differential of (c)'s compiler-consult needs a program whose REAL speculative-BCE guard fails per-bci at runtime (the `DEOPT_EAGER` trigger is an unconditional deopt, not a speculation failure, so it does not exercise the guard-suppression recompile); the unit test covers the sink→registry→filter chain.

---

## Risks and open questions

- **Every-boundary vs loop-headers-only exit maps** (`deopt-osr.md` ~476-479). Emitting an exit map at every canonical boundary maximizes deopt coverage but bloats metadata. Recommendation: loop-headers-first, widen with measurement.
- **OSR-track vs moving GC** (`deopt-osr.md` ~480-485). An OSR'd frame's oops must be precisely relocatable for a moving collector. `CRATONVM_SHADOW_OSR_TRACK` currently regresses bt18 (conservative-pin × precise-move). Resolving that interaction may become a prerequisite for Step 8 under moving GC; a non-moving sweep sidesteps it.
- **Cat-2 / FP — P2 workstream — ✅ DONE (P2.0 + P2.1 + P2.2).** Real-frame resume
  now reconstructs `long`/`double`/`float` slots at full width; see the dedicated
  **Workstream P2** section above for the landed design. P2.0 (the conservative
  wide-local gate) is fully superseded — `can_deopt_resume` no longer excludes
  wide/FP locals; the per-slot `Unsupported`→re-run net carries soundness. Branch
  `feat/deopt-cat2-fp-resume`; gate-off byte-identical; 830 jit-lib + 37 vm-lib
  deopt tests green. Remaining: typed operand-stack cat-2/FP and register-resident
  *oop* typing (both currently re-run safely).
- **Elided monitors on a scalar-replaced object** — see Workstream A caveat: gated off (`can_deopt_resume = false`) when `scalar_replaced` is non-empty or the method is `ACC_SYNCHRONIZED`; monitor re-entry at resume is a later refinement.
- **Epoch / MakeNotEntrant invalidation of baked deopt-point pointers** — boxed `DeoptimizationPoint` pointers are baked into the stub as arg0 (`deopt_boxes`, `x64.rs:6184`). After recompilation/invalidation these must be versioned by `compilation_epoch` and checked before dereference (Step 9), or a stale box could be followed into freed memory.

---

## Validation / acceptance

- **Differential verifier** (backport Step 5, `CRATONVM_DEOPT_VERIFY`, `deopt_verify_enabled()` at `jit/src/lib.rs:869`): for each landed increment, an eager-deopt run must produce interpreter state byte-identical to the never-JIT'd baseline — covering Workstream A's scalar-replace resume and Workstream B's OSR-exit resume.
- **bt18 golden checksum `68332206` unchanged with the gate off** — every increment must remain byte-identical when `CRATONVM_DEOPT_REAL` (and `CRATONVM_SHADOW_OSR_TRACK`) are off. This is the primary non-regression bar.
- **OSR-exit end-to-end** — an OSR-compiled Binary Trees loop instrumented to take a rare branch performs an OSR-exit (reconstruct at the loop bci, resume the loop body) and produces the **same final result** as the un-instrumented run; with the gate off the run is byte-identical to today.
- **Materializer end-to-end** — a method that scalar-replaces an object, deopts under `CRATONVM_DEOPT_REAL`, and resumes via the materialized object yields the same result as the never-JIT'd run; cyclic-graph and GC-during-build variants pass.