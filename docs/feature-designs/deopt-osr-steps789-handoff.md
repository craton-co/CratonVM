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
- **A4 / verifier increment 2 — STILL OWED (the eager-deopt differential).** Force
  a deopt at a *non-failing* guard, reconstruct, and compare the
  reconstructed-interpreter end-result against the JIT result — the form that
  catches a *value* drift (right slot count, wrong contents) the structural layer
  cannot. It needs a forced-deopt injection hook + a side-effect-free (or
  re-run-safe) method set to compare against; it is a harness, not a cheap check.

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

**Remaining: Step 9** (epoch-invalidation consumer for `compilation_epoch` vs
stale baked boxes) is unstarted.

---

## Risks and open questions

- **Every-boundary vs loop-headers-only exit maps** (`deopt-osr.md` ~476-479). Emitting an exit map at every canonical boundary maximizes deopt coverage but bloats metadata. Recommendation: loop-headers-first, widen with measurement.
- **OSR-track vs moving GC** (`deopt-osr.md` ~480-485). An OSR'd frame's oops must be precisely relocatable for a moving collector. `CRATONVM_SHADOW_OSR_TRACK` currently regresses bt18 (conservative-pin × precise-move). Resolving that interaction may become a prerequisite for Step 8 under moving GC; a non-moving sweep sidesteps it.
- **Cat-2 / FP needing `xmm[16]`** (`deopt-osr.md` ~468-472). `SavedRegisters` is `gpr[16]` only; long/double and FP-in-register slots resolve to `Unsupported` → re-run. A loop with a live `double` accumulator cannot OSR-exit until the snapshot is extended to `xmm[16]` plus a width source. Blocks neither Workstream A's integer/object case nor Step 7's emit-and-discard.
- **Elided monitors on a scalar-replaced object** — see Workstream A caveat: gated off (`can_deopt_resume = false`) when `scalar_replaced` is non-empty or the method is `ACC_SYNCHRONIZED`; monitor re-entry at resume is a later refinement.
- **Epoch / MakeNotEntrant invalidation of baked deopt-point pointers** — boxed `DeoptimizationPoint` pointers are baked into the stub as arg0 (`deopt_boxes`, `x64.rs:6184`). After recompilation/invalidation these must be versioned by `compilation_epoch` and checked before dereference (Step 9), or a stale box could be followed into freed memory.

---

## Validation / acceptance

- **Differential verifier** (backport Step 5, `CRATONVM_DEOPT_VERIFY`, `deopt_verify_enabled()` at `jit/src/lib.rs:869`): for each landed increment, an eager-deopt run must produce interpreter state byte-identical to the never-JIT'd baseline — covering Workstream A's scalar-replace resume and Workstream B's OSR-exit resume.
- **bt18 golden checksum `68332206` unchanged with the gate off** — every increment must remain byte-identical when `CRATONVM_DEOPT_REAL` (and `CRATONVM_SHADOW_OSR_TRACK`) are off. This is the primary non-regression bar.
- **OSR-exit end-to-end** — an OSR-compiled Binary Trees loop instrumented to take a rare branch performs an OSR-exit (reconstruct at the loop bci, resume the loop body) and produces the **same final result** as the un-instrumented run; with the gate off the run is byte-identical to today.
- **Materializer end-to-end** — a method that scalar-replaces an object, deopts under `CRATONVM_DEOPT_REAL`, and resumes via the materialized object yields the same result as the never-JIT'd run; cyclic-graph and GC-during-build variants pass.