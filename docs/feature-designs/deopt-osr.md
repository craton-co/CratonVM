# Real-frame deoptimization + precise OSR (entry *and* exit)

Status: design / partially-landed prerequisites. Effort: **XL**, decomposed
below. This doc is the joining piece between two efforts that already exist in
the tree but stop short of each other:

1. **Real-frame deopt** — reconstruct a precise interpreter frame at the
   trapping bci instead of re-running the method from bci 0. The *mechanism*
   landed on the dormant IR path; the production single-pass x64 backend does
   not yet deopt; **virtual-object (scalar-replaced) re-materialization is a
   panic stub**.
2. **OSR (On-Stack Replacement)** — enter JIT code mid-loop from the
   interpreter. The **entry** direction is real, wired, and exercised on hot
   loops (Binary Trees). The **exit** direction — leaving an OSR'd (or any JIT)
   frame mid-loop back to the interpreter at a bytecode index — does **not
   exist**; an OSR'd frame that needs to bail still uses the `i64::MIN` re-run
   sentinel, which is wrong for a frame entered partway through.

The two share one substrate: a *per-PC map from machine state (registers /
spill slots / scalar-replaced objects) to interpreter locals + operand stack*.
Real-frame deopt needs it at guard sites to **exit**; OSR needs the inverse
(interpreter locals → machine state) at loop headers to **enter**. Completing
deopt makes precise OSR-exit nearly free, and the OSR machinery already proves
the map plumbing works end-to-end. This doc designs them together so the map is
built once.

This doc deliberately does **not** restate the full register→slot snapshot and
in-stub trampoline design — those are scoped in
[`real-frame-deopt.md`](real-frame-deopt.md) (IR-path mechanism, landed) and
[`real-frame-deopt-x64-backport.md`](real-frame-deopt-x64-backport.md) (x64
production backport, scoped). It focuses on the two pieces those docs explicitly
defer: **(a) virtual-object re-materialization** and **(b) the OSR-exit /
mid-loop deopt-out path**, plus how to validate both on loop-heavy benchmarks.

## Problem & motivation

Today a JIT method that hits a failed speculation returns the `i64::MIN`
sentinel and the interpreter **re-runs the whole method from bci 0**
(`vm/src/runtime/interpreter.rs:17043` → CacheMiss; slow sink `:17322` →
`Ok(None)`). The surrounding comments (`:17042`, `:17319`) note that re-running
a method with side effects double-executes them. That is the root constraint:

- **Speculative optimization is throttled.** Null-check elision, monomorphic
  inline caches, CHA-based devirtualization, and scalar replacement all need a
  *cheap, correct* fallback when the speculation fails. Without precise frame
  reconstruction the JIT must keep slow paths inline or decline the
  optimization, because re-running is unsound past any side-effecting bytecode.
- **OSR can enter but can never cleanly leave.** A long-running loop is OSR'd
  into JIT code, but if that compiled frame must bail mid-loop (a rare branch,
  a guard, a class load that invalidates an inlined callee), there is no way to
  hand control back to the interpreter *at the current loop bci with the loop's
  live state*. The `i64::MIN` path re-runs the method from entry — but an OSR'd
  frame was entered partway through, so "re-run from entry" is not even
  semantically the same computation (the pre-loop prologue may have side
  effects, and the loop counter state is gone).
- **Scalar replacement is unsafe across a guard.** Escape analysis
  (`jit/src/escape_analysis.rs`) can prove an object non-escaping *along the
  fast path* and elide its allocation — but if a guard failure needs that
  object, deopt must re-materialize it on the heap. `materialize_virtual_objects`
  is a **panic stub** (`jit/src/deopt.rs:749`), so scalar replacement that would
  need to survive a guard is currently off the table.

The payoff is the standard tiered-JIT contract: speculate aggressively on the
fast path, and on the rare guard miss, **rebuild the exact interpreter frame and
continue** — no double execution, no whole-method re-run.

## Current state in the codebase (cited)

### Deopt data model — exists, partially wired

`jit/src/deopt.rs` (1654 lines) defines the whole model and is the canonical
home:

- `FrameValue` (`deopt.rs:73`) — `Int`/`Float`/`Object`/`Register(u8)`/
  `StackSlot(i32)`/`StackSlotRef(i32)`/`VirtualObject(VirtualObjectState)`/
  `Undefined`/`Unsupported`. `StackSlotRef` and `Unsupported` were added by the
  IR-path increment (the oop + cat-2 safety type source); `StackSlot` resolves
  to `Int`, `StackSlotRef` resolves to `Object` (the raw word IS the heap
  pointer), `Unsupported` forces the safe re-run.
- `VirtualObjectState` (`deopt.rs:106`) — `class_id`, `num_fields`,
  `field_values: Vec<FrameValue>`. The recursive descriptor for a
  scalar-replaced object (fields may themselves be `VirtualObject`s).
- `FrameState` (`deopt.rs:121`) — `method_key`, `bci`, `locals`, `stack`,
  `monitors`, `caller: Option<Box<FrameState>>` (inlined caller chain).
- `DeoptimizationPoint` (`deopt.rs:142`) — `native_offset`, `bci`, `reason`,
  `action`, `speculation_id`, `frame_state`. Per-safepoint record.
- Resolver path: `SavedRegisters { gpr: [u64;16] }` (`deopt.rs:778`, `repr(C)`),
  `resolve_value` (`deopt.rs:802`: `Register(r)→gpr[r]`, `StackSlot→*(rbp+off)`,
  `StackSlotRef→Object(*(rbp+off))`), `reconstruct_frame_from_machine_state`
  (`deopt.rs:867`), `ir_deopt_entry` (`deopt.rs:910`, **2-arg**, passes
  `SavedRegisters::default()` — so the `Register` arm has never run live),
  `take_last_deopt` (`deopt.rs:893`, thread-local stash).
- `DeoptimizationLog` (`deopt.rs:173`) — records events, drives
  `should_give_up`/`most_common_reason`/`recommend_action`. Live-usable today.
- `InvalidationManager` (`deopt.rs:346`) — assumption tracking
  (`LeafClass`/`UniqueConcreteMethod`/…) with reverse indices for
  `on_class_loaded`/`on_method_override`. Live-usable today.

**The hole.** `materialize_virtual_objects` is split: a `#[cfg(test)]` body
(`deopt.rs:719`) that hands back **fake monotonic placeholder addresses**
(`0x1000_0000` stepping by `0x100`) used only by a unit test, and a
`#[cfg(not(test))]` body (`deopt.rs:749`) that **`panic!`s** rather than mint a
fake reference on a live path. Its doc (`deopt.rs:695`–`717`) is the canonical
spec for the real fix: allocate via the live TLAB (may GC — the deopt frame must
already be a valid root set), write the header, recursively materialize fields,
patch cyclic back-references, return the real heap address. `count_virtual_objects`
(`deopt.rs:924`) already counts how many slots in a frame need this.

### CompiledMethod deopt plumbing — present, x64 emits none

`jit/src/lib.rs`: `CompiledMethod.deopt_points: Vec<DeoptimizationPoint>`
(`lib.rs:842`), `_deopt_point_boxes` (`lib.rs:849`, stable boxed pointers baked
into guard branches), `find_deopt_point(native_offset)` (`lib.rs:1159`). The IR
lowerer populates these; **x64 has zero references to `deopt_points`** (the
x64-backport doc is exactly the work to change that). There is **no
`can_deopt_resume` flag** on `CompiledMethod` yet.

### Resume path — IR-only, gated, integer-validated

`vm/src/runtime/interpreter.rs:7264` carries the IR-path precise-resume code,
gated by `CRATONVM_IR_DEOPT_RESUME` (`:7270`). Live proof exists for a pure-int
div-by-zero method resuming at the div bci (see `real-frame-deopt.md` banner).
Object/cat-2 arms are unit-validated but **not yet live-exercised**, because the
IR-path *selection* routes object-bearing / side-effecting methods to the
single-pass x64 backend, which has no IR deopt. `CRATONVM_DEOPT_REAL` and
`CRATONVM_DEOPT_VERIFY` named in the x64-backport doc are **proposed, not yet
present** in the tree (grep: only `CRATONVM_IR_DEOPT_RESUME` exists today).

### OSR — entry is real and wired; exit does not exist

OSR **entry** is substantially implemented and exercised:

- **Trigger / policy** (`jit/src/tiered.rs`): `osr_threshold` (`:66`, default
  `10_000`), back-edge counting builds an OSR `CompilationTask` with
  `osr_bci: Some(bci)` (`:552`, `:571`), `osr_compilations` stat (`:474`).
- **Codegen** (`jit/src/x64.rs`): `osr_entry_native: Vec<i32>` (`:5654`) — an
  OSR entry native offset per bytecode PC, **distinct from** the branch-patch
  `pc_to_native` so an OSR entry lands *before* a LICM-hoisted preheader and
  runs the hoist init (`:21620`–`:21628`). LICM-OSR soundness rejects entries
  that would skip a hoisted preheader of an enclosing loop (`:13525`–`:13551`,
  sets `osr_entry_native[pc] = -1`). High-half (long/double) register
  assignments are nulled in the OSR metadata copy (`:21632`–`:21664`).
  `osr_dead_mask` (`:21664`–`:21683`) prevents loading locals dead at the entry
  PC (which would clobber a live local colored to the same register).
- **Entry trampoline** (`jit/src/lib.rs`): `CompiledMethod.osr_enter`
  (`:1384`, takes `vm_ptr`, `jit_locals`, `entry_pc`, `thread_ptr`),
  `can_osr_enter` (`:1366`), cached trampolines keyed by `target_addr`
  (`osr_trampoline_cache` `:1448`), `emit_osr_trampoline` (`:1482`).
- **Driver** (`vm/src/runtime/interpreter.rs`): `try_osr` (`:15807`),
  `try_osr_with_backoff` (`:4075`), `should_try_osr` (`vm/src/runtime/frame.rs:518`),
  OSR-reuse of cached artifacts (`:15894`–`:15917`), and the live enter at
  `:16424` (`compiled.osr_enter(...)`). Back-edge sites all over the dispatch
  loop call `try_osr_with_backoff` (`:4374`, `:4696`, …).
- **Precise OSR-frame GC tracking** is implemented but **default-OFF**:
  `osr_shadow_track_enabled` / `CRATONVM_SHADOW_OSR_TRACK` (`lib.rs:1463`),
  because turning it on currently regresses bt18 (a conservative-pin ×
  precise-move interaction documented in
  `docs/internal/app-jvm-bugs/precise-jit-stack-maps-followups.md` §1; OSR-track
  reaches the partial `68199090`, the selective-promote default reaches the
  correct `68332206`).

What is **missing for OSR**: an **OSR-exit** (a.k.a. mid-loop deopt-out). There
is no path that leaves a running JIT/OSR frame at an arbitrary loop bci and
hands the interpreter the loop's live locals/stack. Searching the tree, every
JIT bail is either entry-time rejection (`osr_entry_native[pc] = -1`,
`can_osr_enter == false`, `try_osr` returns `None`) or the whole-method
`i64::MIN` re-run. OSR-exit is precisely a deopt whose `FrameState.bci` is a
loop bci rather than a guard bci — so it is the *same machinery* as real-frame
deopt, applied at loop back-edges/headers.

## Proposed design

One map, two directions, three new capabilities.

### A. Shared per-PC state map (already mostly built by both efforts)

- **Exit map** (machine → interpreter): the `DeoptimizationPoint.frame_state`
  emitted at each eligible PC. Built on the IR path (landed) and to be built on
  x64 (`real-frame-deopt-x64-backport.md`). Used by deopt-exit *and* OSR-exit.
- **Entry map** (interpreter → machine): `osr_pc_to_native` + the OSR
  trampoline's local/xmm assignment copy (`osr_local_assignments`,
  `osr_xmm_assignments`, `osr_dead_mask`). Already built and wired.

These are inverses of the same regalloc state at a PC. The deliverable here is
to make the *exit map* emission reuse the same canonical-boundary PCs the OSR
*entry map* already trusts (loop headers / block boundaries), so a PC that can
OSR-*enter* can also OSR-*exit*. That keeps both consistent by construction at
the one place drift would otherwise creep in.

### B. Virtual-object (scalar-replaced) re-materialization — the panic stub

Replace `materialize_virtual_objects` (`deopt.rs:749`) with a GC-backed
implementation, threaded a heap/allocator handle from the deopt path (it cannot
run against a borrowed `&FrameState` alone — it mutates the heap and may GC).
For each `FrameValue::VirtualObject(state)` reachable from the reconstructed
frame:

1. **Topologically order** the virtual graph. Scalar-replaced objects can
   reference each other (and cycles are possible, e.g. a node whose field points
   back at a parent). Do a two-phase materialization:
   a. **Phase 1 — shell allocation.** For every virtual object, allocate an
      object of `state.class_id` via the live TLAB/allocator and write its
      header (class id, mark word). **Register each freshly allocated shell as a
      GC root immediately** (a temporary root set owned by the deopt operation)
      so a GC triggered by the *next* allocation cannot reclaim a half-built
      object. This is the load-bearing safety invariant from `deopt.rs:700`–
      `709`.
   b. **Phase 2 — field stores.** With every shell allocated and rooted, fill
      each `state.field_values[i]`. Primitive fields store directly; nested
      `VirtualObject` fields store the shell address resolved in Phase 1
      (handles cycles and shared references with no special-casing); already-real
      `Object`/`Register`/`StackSlot` fields resolve via `resolve_value` and
      store. Apply the correct write barrier per field so the card table / SATB
      log stays consistent with a real `putfield`.
2. **Rewrite the frame.** Replace each `FrameValue::VirtualObject` in the
   reconstructed `locals`/`stack`/`monitors` with the real `Object(addr)` from
   Phase 1. Monitors held on a scalar-replaced object (lock elision) re-enter
   the now-materialized object (see Risk: elided monitors).
3. **Drop the temporary root set** only after the reconstructed interpreter
   `Frame` is fully built and itself a GC root (the frame's locals/stack are
   scanned by the normal interpreter root walk from that point).

Keep the hard guard philosophy: until this lands, any method whose frame
contains a `VirtualObject` at an eligible guard sets `can_deopt_resume = false`
and stays on the `i64::MIN` re-run path. The stub must never be reachable on a
live path returning a fabricated address.

### C. OSR-exit (mid-loop deopt-out)

OSR-exit is real-frame deopt with two specializations:

1. **The resume bci is a loop bci, not a guard bci.** The exit map at a loop
   back-edge / canonical block boundary describes the loop's live state. When a
   running JIT/OSR frame must bail (rare branch taken, guard failed, inlined
   callee invalidated by `InvalidationManager`), it reconstructs a frame at that
   loop bci and resumes interpreting the loop body — no re-run from entry.
2. **The exiting frame may itself be an OSR frame.** Its prologue ran the
   trampoline, not the method entry; "re-run from bci 0" is therefore not just
   slow but *wrong* (pre-loop side effects would re-fire). OSR-exit is the
   *only* correct bail for an OSR'd frame, which is why the `i64::MIN` path must
   be disabled for OSR-compiled methods that don't have an exit map (today they
   are simply never bailed mid-loop; we make that explicit and then provide the
   exit).

Mechanically OSR-exit reuses the deopt-exit trampoline + `reconstruct_frame_from_machine_state`
+ the VM-side `Frame` builder. The new surface is: emit exit maps at loop
boundaries (not only at speculative guards), and a `can_osr_exit` predicate
mirroring `can_deopt_resume` but evaluated at loop headers. Because OSR-entry
already validates these PCs as canonical (clean operand stack, warm slots,
LICM-sound), OSR-exit inherits a vetted PC set for free.

### D. De-speculation policy (already modeled, just wire it)

`DeoptimizationLog::recommend_action` (`deopt.rs:267`) already encodes the
HotSpot-style escalation: first deopt → `Reinterpret`; repeated → recompile;
runaway → `MakeNotEntrant` → `MakeNotCompilable`. Wire each real deopt/OSR-exit
through `record_deopt` + `recommend_action`, and tie `MakeNotEntrant` to a
compilation epoch so stale boxed `DeoptimizationPoint` pointers are never
followed after invalidation (Risk: recompilation invalidation).

## Incremental delivery plan

Each step is independently mergeable and build-green. Steps 1–4 are the
prerequisites already scoped in the two companion docs (listed here for ordering
only); the **new** work in *this* doc is Steps 5–9.

1. *(scoped: x64-backport Steps 1–3)* x64 emits exit maps at the BCE pilot
   guard, in-stub 3-arg deopt entry, reconstruct-and-stash. No resume.
2. *(scoped: x64-backport Step 4)* Flip deopt-exit resume for pure-int,
   side-effecting, non-virtual methods behind `CRATONVM_DEOPT_REAL`. Proves the
   double-exec bug is gone.
3. *(scoped: x64-backport Step 5)* `can_deopt_resume` finalize +
   `CRATONVM_DEOPT_VERIFY` eager-deopt differential verifier.
4. *(scoped: x64-backport Step 6)* Widen to other canonical-boundary
   non-speculative guards; wire `DeoptimizationLog`.

5. **GC-backed `materialize_virtual_objects` — shells + Phase-1 rooting only.**
   Replace the panic stub with: thread an allocator handle from the deopt path;
   allocate + header-init + temporarily-root every shell; **do not** fill fields
   yet — return shells with default-zero fields and leave
   `can_deopt_resume = false` for virtual-bearing frames (so nothing resumes on
   them). *Lands:* a unit/integration test (under `CRATONVM_GC_STRESS`) that a
   frame with N virtual objects allocates N real, rooted, header-valid objects
   that survive a forced GC. *Gate:* additive; virtual frames still don't
   resume. *Risk:* med — rooting-before-next-alloc is the sharp edge, but
   nothing consumes the objects yet.

6. **Phase-2 field stores + cyclic patching; flip virtual resume.** Fill fields
   (primitive, nested-shell, resolved-real), apply write barriers, rewrite the
   frame's `VirtualObject` slots to real `Object`. Allow `can_deopt_resume` for
   virtual-bearing frames (still gated by `CRATONVM_DEOPT_REAL`). *Lands:* a
   method that scalar-replaces an object, deopts past the point of replacement,
   and resumes with a heap object whose fields match the interpreter's
   re-run-from-entry object (differential, via `CRATONVM_DEOPT_VERIFY`); a
   cyclic-graph case (two objects referencing each other). *Risk:* high — first
   heap mutation on the deopt path; de-risked by Step 5 + the verifier.

7. **OSR-exit map emission.** Emit exit maps at loop back-edges / canonical
   block boundaries (reuse the OSR-entry-vetted PC set), add `can_osr_exit`
   mirroring `can_deopt_resume`. Emit-and-discard: nothing exits yet. *Lands:* a
   unit test asserting an exit map exists at a loop header with locals matching
   regalloc and the IV/array-ref typed correctly. *Gate:* additive. *Risk:*
   low — additive, reuses the deopt snapshot emitter.

8. **Flip OSR-exit.** Route a mid-loop bail (start with an uncommon-trap branch
   inside an OSR'd loop) through the deopt trampoline → reconstruct at the loop
   bci → push an interpreter `Frame` resuming the loop body. Disable the
   `i64::MIN` re-run for OSR-compiled methods once `can_osr_exit` holds. *Lands:*
   an OSR'd loop that takes a rare branch resumes interpreting the loop body at
   the correct bci with the correct loop-counter/accumulator state — *not* from
   method entry. *Gate:* `CRATONVM_DEOPT_REAL` (shared). *Risk:* high — first
   mid-loop exit; mitigated by Step 7 + verifier + the canonical-boundary
   restriction.

9. **De-speculation wiring + epoch invalidation.** Route every deopt/OSR-exit
   through `record_deopt` + `recommend_action`; implement
   `MakeNotEntrant`/`MakeNotCompilable` with a compilation epoch so stale boxed
   deopt-point pointers are never followed. *Lands:* a method that deopts past
   threshold is made not-entrant and the next call re-enters cleanly (no use of
   freed boxes); `DeoptimizationLog` reports the rate. *Risk:* med.

## Progress (branch `feat/deopt-osr-scaffolding`)

Landed and build-verified (`cargo check -p cratonvm-jit` + `-p cratonvm-vm`
green; `cargo test -p cratonvm-vm --lib deopt_materialize` = 3 passed) on the
feature branch. All additive and gated **unreachable in production**
(`can_deopt_resume` stays `false`, so nothing resumes / OSR-exits yet):

- **Scaffolding (partial).** `CompiledMethod` gains `can_deopt_resume` /
  `can_osr_exit` / `compilation_epoch` (default `false`/`false`/`0`);
  `DeoptReason::OsrExit`; the read-once `CRATONVM_DEOPT_REAL` /
  `CRATONVM_DEOPT_VERIFY` gates (`jit::deopt_real_enabled` /
  `deopt_verify_enabled`).
- **Steps 5+6 — virtual-object re-materialization (two-phase, cycle-safe).** New
  `vm/src/runtime/deopt_materialize.rs`: `TempRootScope` (RAII temporary GC-root
  set over the thread's `native_pin_roots`) + `materialize_virtual_objects`.
  *Phase 1* allocates + header-inits + **immediately-roots** a shell for every
  distinct virtual object (by `VirtualObjectState::id`) reachable from the frame,
  reading shell addresses back from the in-place-forwarded pin set after the
  optional stress GC (correct under a moving collector). *Phase 2* fills each
  shell's fields — primitive / already-real `Object` / nested `VirtualObject` /
  `VirtualObjectRef` — GC-barrier-correct like `putfield` (SATB pre + post card
  barrier), resolving shared/cyclic references via the Phase-1 shell map, then
  rewrites the frame's top-level slots to real `Object`s. To make sharing/cycles
  representable (the whole point of two-phase), the jit deopt model gained
  `VirtualObjectState::id` + `FrameValue::VirtualObjectRef(id)`. The jit-crate
  `materialize_virtual_objects` panic stub points here. Acceptance tests (live
  VM): `shells_materialize_and_survive_forced_gc` (N shells survive a forced GC
  while pinned), `materializes_primitive_and_object_fields` (field stores + frame
  rewrite), `materializes_two_object_cycle` (A↔B materializes with the
  cross-references wired, under stress GC).

- **x64 deopt-exit Step 1 (companion `real-frame-deopt-x64-backport.md`).** The
  production single-pass x64 backend now records a precise deopt-exit snapshot at
  the speculative-BCE loop-header guard: `Compiler.{deopt_points,deopt_boxes}` +
  `emit_deopt_snapshot_at_guard(bci)` (builds a `FrameState` from regalloc
  provenance — `Register` / `StackSlot` / `StackSlotRef` via the pure, unit-tested
  `frame_value_for_slot`, plus the positive oop source
  `local_oop_masks`/`local_oop_reached`/`stack_oop_marks`), transferred to
  `CompiledMethod` at finalize. EMIT-AND-DISCARD and verified inert:
  `find_deopt_point` / `deopt_points` have no live-path consumer, so the
  `i64::MIN` re-run is unchanged. The primitive width/type source and
  register-resident-oop typing are deferred (gated later by `can_deopt_resume`).

- **x64 deopt-exit Step 2 — in-stub 3-arg trampoline (stash-only).** `x64_deopt_entry`
  (jit `deopt.rs`) mirrors `ir_deopt_entry` but takes the spilled 16-GPR file, so
  `FrameValue::Register(r)` resolves against the **live** register `r` (vs the IR
  path's default-zeros). It stashes `LAST_DEOPT` and returns `i64::MIN` — **no**
  `set_jit_deopt_pending` (the interpreter sink's `take_last_deopt()`-keyed block
  runs first and clears it), so the entry has no vm dependency and lives in the
  jit crate, baked directly by the stub (no `jit-api`/helper-pointer change). The
  frame-deopt stub (in `emit_deopt_stubs`, gated `deopt_real_enabled() && reason==2`)
  spills RAX..R15 into a 128-byte `SavedRegisters` region reserved in the frame
  (`deopt_regs_base`), sets the 3 args (Win RCX/RDX/R8, SysV RDI/RSI/RDX), CALLs
  the entry **before** the epilogue, returns the sentinel. Gate OFF (default) ⇒
  `deopt_regs_size=0` + the uncommon-trap path emits byte-identically. Designed via
  an Understand→adversarial-Verify workflow (the keying, spill-order, and
  ordering were the load-bearing risks). Verified: `x64_deopt_entry` unit tests
  (Register-resolution + null-safety) pass; full jit suite green except a
  PRE-EXISTING dev crash (`intrinsic_arraycopy`, filed separately) — no regression,
  gate-off byte-identical. STASH ONLY — no resume (Step 4); the emitted stub's
  end-to-end execution is not yet driven by a runtime test (needs a BCE compile+invoke
  harness — folds into the Step-4 resume test).

- **x64 deopt-exit Step 3 — build + validate the interpreter Frame at the sink
  (no resume).** At the interpreter deopt sink, under `CRATONVM_DEOPT_REAL`, the
  stashed `ReconstructedFrame` is now turned into a real interpreter `Frame`:
  `ir_deopt_frame_values_with_objects` maps Int + **Object** refs (refusing
  cat-2/`Unsupported`/virtual/FP/unresolved → re-run); `build_deopt_frame_inner`
  GC-roots the reconstructed oops in `native_pin_roots` **before**
  `refill_pools_from_shared` (whose `acquire()` may GC), **re-reads** each oop
  from its forwarded pin slot after refill (moving-GC correct), builds the Frame
  via `Frame::new_pooled` at the trapping bci, and returns it; the wrapper
  `build_validate_discard_ir_deopt` discards it and STILL re-runs (CacheMiss) —
  a live soak of the reconstruction/rooting/build machinery, no resume yet.
  Designed via an Understand→adversarial-Verify workflow whose GC-rooting
  reviewer caught the real bugs (cache-forwarded-ref staleness → re-read from
  pins + stress-GC only before refill; `debug_assert` pin-leak → single
  guaranteed truncate, validation moved to tests). Gate-OFF (default):
  byte-identical (the build is inside `if deopt_real_enabled()`; the existing
  `CRATONVM_IR_DEOPT_RESUME` int-resume path is untouched). Verified: 4 unit
  tests pass (mapper; refuse-unmappable-without-pin-leak; build int+object frame;
  **Object survives a forced GC during the build**); vm lib compiles green, no
  new warnings; `memory::roots` tests pass in isolation (the full-suite failures
  are pre-existing parallel-test pollution, surfaced only because the `oscache`
  compile-break — `task_f849e93a` — was temp-patched to run the suite, and reverted).

- **x64 deopt-exit Step 4 — FLIP THE RESUME.** The payoff: under `CRATONVM_DEOPT_REAL`
  the sink now RESUMES an Object-bearing deopt at the trapping bci
  (`resume_real_ir_deopt`: build the frame → `push_frame_and_fire_entry` →
  return `FramePushed`) instead of re-running the whole method from entry —
  killing the side-effect double-execution. The load-bearing **GC-rooting
  handoff**: the temporary `native_pin_roots` pins are held ACROSS the push (so
  the oops are rooted by the pins, then by both pins and frame) and released only
  AFTER the frame is on `thread.frames` (its locals/stack are then GC roots), so
  there is no unrooted window. Designed via an Understand→adversarial-Verify
  workflow whose 3 reviewers (GC-handoff/UAF, sink control-flow, gate/re-entrancy)
  each independently returned **"ship as-is"** — confirming no unrooted window,
  consistent moving-GC forwarding, correct skip-of-saved-args on resume
  (the args are re-homed in the resumed frame's locals), two default-off gates,
  no int-path overlap, no stale `LAST_DEOPT`. The one non-blocking finding (a
  pooled-buffer leak on the unreachable `.ok()?` bail) is hardened
  (`frame.recycle` before bail). Gate-OFF (default): byte-identical. Verified: 5
  unit tests pass, incl. the load-bearing **`resumed_frame_roots_oops_after_pin_release`**
  (force a GC AFTER push+pin-release → the oop survives via the pushed frame).
  STILL OWED: the end-to-end BCE compile→invoke runtime test (drive the Step-2
  stub → `x64_deopt_entry` → sink for real) is infra-blocked — the jit crate's
  `#[cfg(feature="vm-tests")]` tests reference a non-existent `crate::vm::SharedVm`
  (can't compile), and the gate override doesn't cross the jit↔vm boundary; it
  needs a `pub` (non-`cfg(test)`) override + a cratonvm_gc-direct array, or a full
  vm-crate invoke. The resume *correctness* (the UAF-risk) is unit-tested +
  3-way adversarially verified; only the through-the-JIT execution path is unproven.

Since landed (branch `feat/deopt-osr-completion`, see the handoff doc for detail):
deopt-osr **Steps 7–8** (OSR-exit map emission + loop-bci resume flip + OSR-driver
safety); **Workstream A** (`materialize_virtual_objects` wired into
`build_deopt_frame_inner` so virtual-bearing frames materialize + resume); and
x64-backport **Step 5** (`can_deopt_resume` coverage-gate finalize + sink consult).

Not yet done: the **end-to-end BCE runtime test** (above, infra-blocked); the
x64-backport **`CRATONVM_DEOPT_VERIFY` eager-deopt differential verifier** (the
gate exists but has no consumer; also closes Workstream A4) and **widening guards
beyond the BCE pilot**; **Step 9** epoch-invalidation (`compilation_epoch`
consumer for stale baked deopt-point boxes after `MakeNotEntrant`); the **Step 8
follow-up** true OSR-exit state transfer into the live interpreter frame (coupled
to `CRATONVM_SHADOW_OSR_TRACK` / moving-GC OSR tracking); and **cat-2 / FP
`xmm[16]`** snapshot extension (long/double/FP-in-register slots still resolve to
`Unsupported` → re-run).

## Risks & open questions

- **GC during materialization (Step 5/6).** A GC between shell allocations with
  an un-rooted half-built object corrupts the heap. The Phase-1 temporary root
  set is the mitigation; it must be installed *before* the allocation that could
  trigger the GC, and dropped only after the reconstructed frame is itself a
  root. This is co-designed with `default-moving-young-gen.md` (precise JIT
  roots and deopt share the register→oop map).
- **Write barriers on materialized fields.** Storing a young object reference
  into a materialized object must go through the same card-table / SATB barrier
  a real `putfield` would, or a later GC misses the cross-generational edge.
- **Elided monitors (lock elision over scalar-replaced objects).**
  `monitorenter`/`exit` emit nothing under scalar replacement; a resumed frame's
  `monitorexit` would hit an un-entered monitor. Phase-A gate keeps
  `can_deopt_resume = false` when `scalar_replaced` is non-empty *or*
  `ACC_SYNCHRONIZED`; re-entering monitors on the materialized object is a
  later refinement to be done together with Step 6.
- **OSR-exit on an OSR'd frame must never fall back to `i64::MIN` re-run.**
  Re-running from entry re-fires pre-loop side effects. The plan disables the
  sentinel path for OSR-compiled methods only once `can_osr_exit` is true; until
  then those frames are simply never bailed mid-loop (today's behavior — they
  run to loop completion or throw).
- **Map drift (exit map vs regalloc).** A one-slot disagreement silently
  restores garbage. Mitigated by the mandatory `CRATONVM_DEOPT_VERIFY`
  eager-deopt differential verifier and by emitting exit maps only at the same
  canonical PCs OSR-entry already trusts (snapshot and codegen read the same
  state at the same PC).
- **Cat-2 / FP slots.** `SavedRegisters` is `gpr[16]` only; long/double and
  FP-in-register slots resolve to `Unsupported` → re-run. Real-frame resume of
  those needs an `xmm[16]` extension + a width source (a follow-up shared with
  the x64-backport doc). OSR-exit of a loop with a live `double` accumulator is
  blocked until then.
- **Recompilation invalidation.** Boxed `DeoptimizationPoint` pointers encode a
  specific regalloc; `MakeNotEntrant` must version them by compilation epoch and
  assert each point's region ⊆ the owning `CompiledMethod` code range.
- **Open: emit exit maps at *every* canonical boundary, or only loop headers?**
  Every-boundary maximizes OSR-exit/deopt coverage but inflates metadata;
  loop-headers-only is the minimal set that makes OSR-exit work. Recommend
  loop-headers-first, widen with measurement.
- **Open: does OSR-track (`CRATONVM_SHADOW_OSR_TRACK`) become a prerequisite for
  OSR-exit under a moving GC?** An OSR'd frame's live oops must be precisely
  relocatable for the exit's reconstructed object refs to stay valid. The
  followups doc shows OSR-track currently regresses bt18; resolving that
  interaction (precise-move × conservative-pin) may gate Step 8 under moving GC,
  though the default selective-promote non-moving sweep sidesteps it.

## Validation / acceptance

The bar throughout: **byte-for-result-identical to HotSpot**, cross-checked on
loop-heavy and side-effecting workloads.

- **Unit / differential.** Extend `deopt.rs` tests: virtual-object shell count
  + survives-GC (Step 5), field-fill + cyclic-graph equality vs an
  interpreter-built object (Step 6), exit-map-exists-at-loop-header (Step 7).
  The `CRATONVM_DEOPT_VERIFY` eager-deopt verifier (deopt at *every* eligible
  guard/loop boundary, reconstruct, compare reconstructed-interpreter result vs
  JIT result) is CI-mandatory before any guard/loop family is flipped, and a
  deliberately one-slot-shifted snapshot must be caught.
- **Uncommon-trap guards.** A method with an elided null check / monomorphic
  inline cache / scalar-replaced object that deopts on the rare path must resume
  at the trapping bci with state identical to never-speculating execution, and
  must **not** double-execute side effects (the exact bug today). Verify with a
  method whose pre-guard bytecode has an observable side effect.
- **Loop-heavy benchmarks (Binary Trees).** Binary Trees (`bt16`/`bt18`) is the
  primary OSR workload (the followups doc tracks its golden checksum
  `68332206`). Acceptance: (a) `bt18` checksum stays `68332206` (cross-check
  `java -cp bench BenchSuite` per memory) with deopt/OSR-exit enabled; (b) an
  OSR'd Binary Trees `binaryTrees`/`check` loop that takes an instrumented rare
  branch performs an OSR-exit and the result is unchanged; (c) no fast-path
  throughput regression on the non-deopting path (the exit map is metadata, not
  code on the hot path). Measurement harness:
  `docs/internal/app-jvm-bugs/reference_bintrees_measurement_loop` (8g heap,
  taskkill stray cratonvm first, build via `build-cpu.bat`).
- **App-suite smoke.** A representative kafka/h2/Tomcat method that today
  re-runs on a bounds/null guard shows a precise resume under `CRATONVM_DEOPT_REAL`,
  with no regression to the gated-off default.

## Scaffolding to land first (minimal compiling additions)

These are the smallest changes that compile green and unblock the steps above —
described here, not implemented in this doc.

- **`CompiledMethod.can_deopt_resume: bool` and `can_osr_exit: bool`**
  (`jit/src/lib.rs`), default `false`, set by the finalizer. Mirrors the
  existing `fully_oop_covered` coverage-gate pattern (`x64.rs:21206`). Until the
  emitters populate them, every method stays on the `i64::MIN` path — pure
  additive, no behavior change.
- **`CRATONVM_DEOPT_REAL` env gate** (read-once cached, default-OFF), consulted
  identically at both interpreter deopt sinks (`interpreter.rs:17043`, `:17322`)
  and at the OSR-exit sink. Shared by deopt-exit *and* OSR-exit so the two never
  run half-on. (Named in the x64-backport doc; not yet in the tree.)
- **`CRATONVM_DEOPT_VERIFY` env gate** (test/CI, default-OFF) for the eager-deopt
  differential verifier.
- **Allocator/heap handle parameter** on a new
  `materialize_virtual_objects(frame, heap)` signature (replacing the panicking
  stub), plus a `TempRootScope` RAII helper (in `vm/src/runtime/`) that pins a
  set of freshly allocated oops as GC roots for the duration of materialization
  and unpins on drop. Stub the body to `unimplemented!()` behind
  `can_deopt_resume == false` so it compiles but is never reached live until
  Step 5/6.
- **`emit_osr_exit_map_at(loop_bci)`** stub on the x64 `Compiler` (parallel to
  the proposed `emit_deopt_snapshot_at_guard`), emitting an exit
  `DeoptimizationPoint` keyed at a loop boundary and discarded until Step 8. Add
  `Compiler.osr_exit_points` + boxed-pointer vec mirroring `deopt_points` /
  `_deopt_point_boxes`.
- **`DeoptReason::OsrExit`** variant (`deopt.rs:27`) so OSR-exit events are
  distinguishable in `DeoptimizationLog` / `recommend_action` (treat like
  `UncommonTrap` for policy initially).
- **Compilation-epoch field** on `CompiledMethod` (a monotonic `u64`) so
  `MakeNotEntrant` can version deopt points and the resume path can assert the
  box it is about to follow belongs to a still-entrant epoch.
