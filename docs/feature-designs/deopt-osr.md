# Real-frame deoptimization + precise OSR (entry and exit)

**Status:** Shipped (default on; `CRATONVM_DEOPT_REAL=0` opts out).

A guard or loop bail **resumes at the trapping bci** instead of re-running the
whole method. The shared per-pc state map, the virtual-object
re-materialization consumer, de-speculation with epoch invalidation, and the
category-2 / floating-point resume are all live.

Two companions stay default-off, so an OSR exit takes the safe reject path:
`CRATONVM_OSR_EXIT_TRANSFER` (in-place OSR-exit transfer) and
`CRATONVM_SHADOW_OSR_TRACK` (moving-GC OSR-frame tracking).

**Footprint cost:** every JIT frame reserves an extra 256 B for the
`SavedRegisters` deopt region while the feature is on.

This document is the combined design. The per-feature current state lives in
[`real-frame-deopt.md`](real-frame-deopt.md),
[`jit-osr-entry-metadata.md`](jit-osr-entry-metadata.md) and
[`jit-osr-exit-and-recompile.md`](jit-osr-exit-and-recompile.md).

## Problem & motivation

Today a JIT method that hits a failed speculation returns the `i64::MIN` sentinel and the interpreter **re-runs the whole method from bci 0** (`vm/src/runtime/interpreter.rs` -> CacheMiss / slow sink). Re-running a method with side effects double-executes them. That is the root constraint:

- **Speculative optimization is throttled.** Null-check elision, monomorphic inline caches, CHA-based devirtualization, and scalar replacement all need a *cheap, correct* fallback when the speculation fails. Without precise frame reconstruction the JIT must keep slow paths inline or decline the optimization, because re-running is unsound past any side-effecting bytecode.
- **OSR can enter but can never cleanly leave.** A long-running loop is OSR'd into JIT code, but if that compiled frame must bail mid-loop (a rare branch, a guard, a class load that invalidates an inlined callee), there is no way to hand control back to the interpreter *at the current loop bci with the loop's live state*. Re-running from entry is semantically incorrect because the pre-loop prologue may have side effects, and the loop counter state is gone.
- **Scalar replacement is unsafe across a guard.** Escape analysis (`jit/src/escape_analysis.rs`) can prove an object non-escaping *along the fast path* and elide its allocation — but if a guard failure needs that object, deopt must re-materialize it on the heap.

The payoff is the standard tiered-JIT contract: speculate aggressively on the fast path, and on the rare guard miss, **rebuild the exact interpreter frame and continue** — no double execution, no whole-method re-run.

---

## Current state & codebase citation

### 1. Deopt data model (`jit/src/deopt.rs`)
- `FrameValue` — `Int`/`Float`/`Object`/`Register(u8)`/`StackSlot(i32)`/`StackSlotRef(i32)`/`VirtualObject(VirtualObjectState)`/`VirtualObjectRef(usize)`/`Undefined`/`Unsupported`/`Double(u64)`/`RegisterLong(u8)`/`XmmFloat(u8)`/`XmmDouble(u8)`/`StackSlotFloat(i32)`/`StackSlotDouble(i32)`.
- `VirtualObjectState` — `id`, `class_id`, `num_fields`, `field_values: Vec<FrameValue>`. Recursive descriptor for a scalar-replaced object (handles cycles/references via `id` / `VirtualObjectRef`).
- `FrameState` — `method_key`, `bci`, `locals`, `stack`, `monitors`, `caller: Option<Box<FrameState>>` (inlined caller chain).
- `DeoptimizationPoint` — `native_offset`, `bci`, `reason`, `action`, `speculation_id`, `frame_state`. Per-safepoint record.
- `DeoptimizationLog` — records events, drives `should_give_up`/`most_common_reason`/`recommend_action`.
- (Deleted 2026-09-12: `InvalidationManager` / `CompilationAssumption`. No compile path ever registered an assumption, so it evicted nothing. A body's dependency record is its own `inlined_methods`; a compile racing an invalidation is refused by the cache's invalidation log in `JitCache::put`.)

### 2. CompiledMethod deopt plumbing (`jit/src/lib.rs`)
- `CompiledMethod` fields: `deopt_points: Vec<DeoptimizationPoint>`, `can_deopt_resume: bool`, `can_osr_exit: bool`, `compilation_epoch: u64`, `osr_exit_points: Vec<usize>`.
- `deopt_real_enabled()` (`CRATONVM_DEOPT_REAL`) and `deopt_verify_enabled()` (`CRATONVM_DEOPT_VERIFY`) read-once env gates.

### 3. OSR-entry substrate (`jit/src/x64.rs` & `vm/src/runtime/`)
- **Canonical loop-boundary PC set**: `osr_entry_native: Vec<i32>` is positive at boundary PCs outside all LICM-hoisted loop bodies. OSR-exit reuses this vetted PC set.
- **Entry trampoline**: `CompiledMethod.osr_enter` takes `vm_ptr`, `jit_locals`, `entry_pc`, `thread_ptr`.
- **Driver**: `try_osr_with_backoff` orchestrated from the dispatch loop.

---

## Detailed Design & Implementation

### A. Shared per-PC state map
These are inverses of the same regalloc state at a PC:
- **Exit map** (machine -> interpreter): `DeoptimizationPoint.frame_state` emitted at each eligible PC. Used by deopt-exit and OSR-exit.
- **Entry map** (interpreter -> machine): `osr_pc_to_native` + OSR trampoline local/XMM assignment copy.

To keep both consistent, the *exit map* emission reuses the same canonical-boundary PCs that OSR-entry already trusts (loop headers / block boundaries outside hoisted loops).

### B. Virtual-object (scalar-replaced) re-materialization
Implemented in [deopt_materialize.rs](../../vm/src/runtime/deopt_materialize.rs) via `materialize_virtual_objects`. It mutates `ReconstructedFrame` in-place, rewriting every `VirtualObject`/`VirtualObjectRef` slot to a real `FrameValue::Object(addr)`.

1. **Two-phase, cycle-safe materialization**:
   - **Phase 1 — shell allocation**: Collects all distinct `VirtualObjectState::id`s reachable from the frame. Allocates a pinned shell per id via `TempRootScope::alloc_shell` (TLAB allocation + class/mark header initialization).
   - **Phase 2 — field stores**: Fills shell fields via `field_value_to_value` + `store_field_barriered` (real `putfield` SATB/card barrier). Handles cyclic/shared references by retrieving shell addresses from Phase 1.
2. **GC-rooting handoff**:
   - Shell pins must be rooted continuously from allocation until the resumed frame is on the stack.
   - `materialize_virtual_objects` takes a `keep_pins` parameter. On the live path (`keep_pins = true`), it `mem::forget`s the scope so shell pins persist in `native_pin_roots`. The resume sink owns releasing these pins via its existing `pin_base..truncate` window after the frame is pushed onto `thread.frames`.

### C. OSR-exit (mid-loop deopt-out) & True OSR-exit transfer
OSR-exit is a real-frame deopt specialized for loop BCIs:
1. **Emit exit maps**: Done on the x64 `Compiler` via `emit_osr_exit_map_at` at loop boundaries, tagging the points with `DeoptReason::OsrExit`.
2. **True OSR-exit transfer**: `transfer_osr_exit_into_live_frame` (`vm/src/runtime/interpreter.rs`) performs an in-place mutation of the active interpreter frame (overwriting locals + operand stack) rather than a full frame rebuild-and-swap. This preserves the frame's identity and metadata. It maps inputs before writing to allow a safe rollback if an unsupported type is hit.
3. **OSR Driver Safety**: If OSR-entered JIT code bails, the driver intercepts the `i64::MIN` sentinel, checks `take_last_deopt()`, and rejects the OSR (falls back to interpreter execution) to avoid corrupting values or looping indefinitely. Once `can_osr_exit` holds, whole-method re-runs via `i64::MIN` are disabled for OSR-compiled methods.

### D. De-speculation & Epoch Invalidation
- **De-speculation**: Every deopt/OSR-exit routes through `real_frame_deopt_resume_and_despeculate`. It logs the event, evicts the artifact from `jit_cache` (making it not entrant so subsequent calls trigger recompilation), and escalates to `MakeNotCompilable` if the deopt rate exceeds a threshold.
- **Epoch Invalidation**: To prevent stale boxed `DeoptimizationPoint` pointers from being followed after recompilation, `SharedVm.method_epochs` tracks live epochs. Fresher artifacts are stamped with the live epoch at install. Before resuming, the sink asserts `compiled.compilation_epoch >= live_epoch(M)`.

#### Step-9 follow-ups (done, gated `CRATONVM_DEOPT_REAL`, gate-off byte-identical; bt18 = `68332206` gate-off **and** gate-on)
- **In-entry before-deref epoch check** (`CRATONVM_JIT_FREE_CODE=1`): the epoch comparison `method_epochs` enables is now also done *inside the deopt trampoline, before the box is dereferenced*. A process-lifetime-retained `DeoptEpochGuard { creation_epoch, live_epoch_cell }` (`jit/src/deopt.rs`) is baked as a 4th arg into every frame-deopt stub and stamped by the VM at install (`CompiledMethod::stamp_deopt_epoch_guard`); `method_epochs` is now `FxHashMap<String, Box<AtomicU64>>` so the live cell has a stable address (`SharedVm::live_epoch_cell_ptr`). `x64_deopt_entry` consults the guard FIRST and, on a superseded artifact, stashes a `bci = u32::MAX` re-run sentinel **without** touching the (possibly-freed) box. Deopt-point boxes are now retained even under `CRATONVM_JIT_FREE_CODE=1`, so the baked box pointer can never dangle.
- **Eager recompile re-queue**: on a `RecompileAndReinterpret` action, `DeoptimizationController::deoptimize` (`vm/src/jit/helpers.rs`) eagerly enqueues a high-priority `CompilationTask` when a background compiler is active, instead of waiting for the method to re-cross the interpreter hotness threshold. No-op (no queue leak) when no worker drains it — the hotness-retry path still recompiles, so behaviour never regresses.
- **Per-bci de-spec**: a process-global `(method_key, bci)` registry (`deopt::despec_insert`/`despec_contains`, allocation-free when empty) plus `DeoptimizationLog::deopt_count_at_bci`. Once a single speculation site has deopted ≥ `PER_BCI_DESPEC_LIMIT` (4, HotSpot `PerBytecodeTrapLimit`-like), the sink records it and the optimizing backend drops *that* speculative-BCE loop-header guard on recompile (`method_key` plumbed through `compile_with_param_slots`) — so one pathological site is de-spec'd and the method stays compilable instead of a whole-method blacklist (diffuse deopts still hit the per-method backstop). Skipped for the superseded-sentinel bci so a stale artifact never de-specs a non-current speculation.

### E. Cat-2 & FP Real-frame Resume
Allows methods with `long`, `double`, and `float` variables to resume:
1. **SavedRegisters expansion**: Expanded to 256 bytes to store `xmm: [u64; 16]` in addition to GPRs.
2. **Width classification**: `classify_local_kinds` scans method opcodes to build `Compiler.local_kinds`, mapping locals to specific widths/types (`Int`, `Long`, `Float`, `Double`, `Ref`, `HighHalf`, `Ambiguous`, `Unknown`).
3. **Sound fallback**: Provenance-kind contradictions or `Ambiguous` slots resolve to `Unsupported` to trigger a safe whole-method re-run instead of silent corruption.

---

## Incremental delivery plan & Progress status

All steps are merged and build-green on dev:

- [x] **Step 1: x64 BCE Pilot Guard Snapshot**: Emissions of exit maps at BCE pilot guard, in-stub deopt entry, and frame reconstruction.
- [x] **Step 2: GPR Spill & Stash**: Frame-deopt stub spills RAX..R15 into `SavedRegisters` and stashes `LAST_DEOPT`.
- [x] **Step 3: Reconstruct & Root**: Interpreter sink builds `Frame` at trapping bci, rooting oops in `native_pin_roots` before refill.
- [x] **Step 4: Flip the Resume**: Sink resumes Object-bearing deopts at trapping BCI; implements the GC-rooting handoff.
- [x] **Step 5: Virtual Shell Allocation**: Two-phase shell allocation & GC rooting in `materialize_virtual_objects`.
- [x] **Step 6: Virtual Field Stores**: Field fills, cyclic patching, Card/SATB barriers, and resume enablement.
- [x] **Step 7: OSR-exit Map Emission**: Exit maps recorded at OSR-vetted loop boundaries.
- [x] **Step 8: Flip OSR-exit & Transfer**: Loop-bci resume flip, OSR-driver safety integration, and true OSR-exit state transfer.
- [x] **Step 9: De-speculation & Epochs**: `DeoptimizationLog` integration, compilation epoch bumping and invalidation.
- [x] **Workstream P2: Cat-2 & FP Resume**: Typed local classification, XMM register spilling, and resolver support.

---

## Risks & open questions

- **Elided monitors on scalar-replaced objects**: `can_deopt_resume` is kept `false` (forces re-run) when the compilation's `scalar_replaced` set is non-empty or the method is `ACC_SYNCHRONIZED`. Monitor re-entry at resume remains a future refinement.
- **Every-boundary vs loop-headers-only exit maps**: Emitting exit maps at every canonical boundary maximizes coverage but inflates metadata. We default to loop-headers-first.
- **OSR-track vs moving GC**: Relocating live oops in an OSR frame requires `CRATONVM_SHADOW_OSR_TRACK`, which currently regresses `bt18`. Non-moving sweep configurations sidestep this, but resolving the precise-move × conservative-pin interaction is a prerequisite for making OSR-exit default-on under a moving young generation.

---

## Validation / verification plan

- **Automated Tests**:
  - VM deopt tests (`vm/src/runtime/deopt_materialize.rs` and `interpreter.rs` step3 tests): covers virtual materialization, cyclic object graphs, GCs during build, and epoch bumps.
  - JIT tests (`jit/src/deopt.rs` & `x64.rs` classifier/resolver tests).
- **Differential Verifier (`CRATONVM_DEOPT_VERIFY`)**:
  - `verify_reconstructed_frame` checks structural invariants (slot counts, malformed virtual descriptors, dangling references).
  - `verify_reconstructed_oops` asserts that object reference slots are either null or contain a valid heap address, acting as a runtime fail-safe.
- **Live Through-JIT Eager Deopt Validation**:
  - Under `CRATONVM_DEOPT_EAGER = 1`, the JIT records snapshots at the first loop header and unconditionally branches to the deopt stub. Verified with separate-process differential checks on `NoArrayDeopt` and `bt18` workloads.
- **Remaining verification follow-up**:
  - **Eager-deopt value differential**: Blocked by read-once cached env gates. Requires adding a forced-BCE-deopt gate in `emit_deopt_stubs` and executing a separate-process runner comparing gate-on vs gate-off stdout/checksums.
