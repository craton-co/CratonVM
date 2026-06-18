# Wire the Tiered Compilation Manager

> **Increment 1 (tier recommendation wired + background compile thread) landed.**
> Steps 1–2 of the ordered plan below are now real:
> - The interpreter no longer discards the recommended tier
>   (`vm/src/runtime/interpreter.rs` ~14361–14400). `on_method_invocation` now
>   *enqueues* a `CompilationTask` at the policy-recommended tier when the C1/C2
>   thresholds are crossed (the result is bound to `recommended_tier` and acted
>   on, not dropped into `_`).
> - A real **background compile thread** now drains the queue off the mutator
>   thread. `jit/src/tiered.rs` grew a shared `CompilerCore` (per-method state +
>   queue + stats + wake condvar + shutdown flag behind an `Arc`), a
>   `BackgroundCompiler` worker handle (RAII: drop → drain+join, so no thread
>   outlives the VM), `start_background_compiler(compile_fn)`, and a process-
>   global `ensure_background_compiler(mgr, make_compile_fn)` started once via
>   `Once` from the interpreter hook. The worker blocks on the condvar (no spin),
>   dequeues highest-priority first, runs the compile callback off-thread, then
>   publishes the tier via `CompilerCore::complete_task`.
> - **Scope note:** for this increment the worker runs a *drain-only* compile
>   closure and the mutator keeps its existing inline `try_jit_upgrade_with_gate`
>   path, because the real codegen callback needs VM-init / class-metadata access
>   that lives outside this item's subsystem boundary. Wiring the real
>   `compile_fn` and switching the mutator to enqueue-only (removing the inline
>   stall) is the remainder of step 2 / step 3.
> - Test: `background_worker_drains_enqueued_task_off_thread` in
>   `jit/src/tiered.rs` (deterministic via an `mpsc` sync handle — asserts the
>   task is compiled on a *different* `ThreadId` than the caller).
> - Step 5 (precise OSR) remains gated on `real-frame-deopt.md` state maps.

Status: design / not started. L. `jit/src/tiered.rs` is a full HotSpot-style
tiered policy + priority compilation queue that the interpreter currently
**throws away** — it compiles exactly one tier at one fixed invocation
threshold, synchronously, on the calling thread.

## Goal

Turn the existing tiered policy into a real two-tier pipeline: a fast **C1**
(quick, unoptimized, profiling) tier and a profile-driven **C2** (the current
optimizing JIT), with a **background compilation thread** draining the existing
`CompilationQueue`, and with the **OSR / back-edge** path actually firing
on-stack replacement for hot loops.

## Current state (cited)

The policy engine is complete and unit-tested; almost none of it is on a live
path.

- **Only one tier, one threshold, synchronous.** The interpreter's JIT trigger
  (`vm/src/runtime/interpreter.rs:14160`–`14186`) reads a single
  `jit_invocation_threshold` (default 500, `CRATONVM_JIT_THRESHOLD`), and once
  crossed calls `try_jit_upgrade_with_gate` **inline on the calling thread**
  (`:14186`) producing the full optimizing backend in one shot. `JIT_RETRY_STRIDE
  = 64` (`:14165`) re-attempts on failure. There is no C1; cold methods jump
  straight from interpreter to the C2-equivalent backend.
- **The tiered manager is consulted, then ignored.** `interpreter.rs:14181`:
  ```
  let _recommended_tier = shared.tiered_manager.on_method_invocation(&tiered_key);
  ```
  The recommended tier is bound to `_` and discarded. The only other live use is
  `vm/src/vm/vm_init.rs:3342` (`on_deoptimization`), which records the event for
  the give-up policy.
- **`on_backedge` is never called.** `tiered.rs:366` implements OSR triggering
  (back-edge count ≥ `osr_threshold`, enqueues a `High`-priority
  `CompilationTask` with `osr_bci: Some(bci)` at `:392`), but **no VM code calls
  `on_backedge`** (grep over `vm/src/` finds zero call sites). So a hot loop in a
  cold method never OSR-compiles — it must wait until the *whole method* is
  invoked `osr_threshold` times, which for a `main()`-style loop never happens.
- **The `CompilationQueue` is never drained.** `tiered.rs:232`–`280` is a
  full three-priority (`High`/`Normal`/`Low`) `VecDeque` queue with
  `enqueue`/`dequeue`; `CompilationTask` (`:222`) carries `target_tier`,
  `priority`, and `osr_bci`. `enqueue_compilation` exists
  (`tiered.rs:480`) and there's a `background_compiler_active`
  flag (`:617`), but **nothing dequeues** — there is no background thread, and
  the VM never enqueues. Tasks would accumulate and never compile.
- **Policy is sound.** `CompilationPolicy` (`tiered.rs:58`) has
  `c1_threshold: 200`, `c2_threshold: 5_000`, `osr_threshold: 10_000`,
  `c2_min_invocations: 1_000`; `should_compile` (`tiered.rs:654`) and
  `CompilationStats` (`tiered.rs:288`, tracks c1/c2/osr/deopt/bailout counts)
  are implemented and tested. The C1↔C2↔Interpreter transition graph is in the
  module doc (`tiered.rs:18`).

Net: a complete policy + queue with no executor and no real C1 tier behind it.

## Design

### 1. A real C1 (fast) tier

C2 today is the single-pass `jit/src/x64.rs` backend plus (gated) the IR
pipeline. "C1" should be a **strictly faster, simpler** compile:

- Reuse the single-pass `x64.rs` backend but **disable** the expensive passes:
  no IR lowering, no escape analysis, no reassociation, minimal regalloc. The
  goal is a quick template-style compile that's faster than the interpreter but
  cheap to produce.
- C1 collects **profiles** (branch bias, receiver types, null frequency) into
  the existing `jit/src/profile.rs` / `tiered.rs` profile structures so C2 can
  use them — this is `CompilationTier::C1WithProfiling` (`tiered.rs:45`).
- C2 stays the current optimizing backend, now **profile-driven**: it consumes
  the C1-collected profile to bias inlining, branch layout, and (once
  `real-frame-deopt.md` lands) speculative guards.

Practical first cut: make "C1" = the existing single-pass backend with
optimizations off, and "C2" = the existing IR/optimized path. The two
already coexist in `lib.rs::try_compile`; the work is selecting between them by
tier instead of by the current ad-hoc gates.

### 2. Background compilation thread

- Spawn one (or a small pool of) compiler thread(s) at VM init that loop:
  `tiered_manager.dequeue()` → compile the `CompilationTask` at its
  `target_tier` (and `osr_bci` if set) → publish the compiled code → flip the
  invoke cache / OSR entry. Use the `background_compiler_active` flag
  (`tiered.rs:617`) to gate the loop.
- The interpreter's trigger block (`interpreter.rs:14174`) changes from "compile
  inline now" to "**enqueue** a task at the manager's recommended tier and keep
  interpreting" — removing JIT compilation from the mutator's critical path
  (today a hot method stalls its first caller for the whole compile).
- Synchronization: the queue is already `Mutex`-guarded; the compiler thread
  needs read access to the class/method metadata it compiles. Reuse the
  `RedefineGate` snapshot the inline path already threads (`interpreter.rs:14186`)
  so a `redefine_class` invalidates a queued/just-compiled task.

### 3. OSR / back-edge wiring

- Call `tiered_manager.on_backedge(key, bci)` from the interpreter's back-edge
  handler (the `goto`/`if_*`-backward and `*return`-loop sites). When it returns
  a `CompilationTask` with `osr_bci`, enqueue it.
- The background thread compiles an **OSR entry**: a version of the method
  entered at `osr_bci` with the live interpreter locals/stack transferred into
  the compiled frame. This is the mirror image of deopt (frame *materialization*
  vs frame *capture*) and shares the same per-bci state map machinery from
  `real-frame-deopt.md`. Until that lands, OSR can be a simpler "compile whole
  method, re-enter at top of loop after the current iteration" approximation.
- **Threshold fixes**: the current single threshold (default 500 invocations)
  has no loop awareness. With `on_backedge` wired, a long-running loop in a
  rarely-*invoked* method finally compiles. Tune `osr_threshold` (default
  10_000) and `c1_threshold` (200) against the app gauntlet — HotSpot's
  defaults are a reference but CratonVM's compile cost differs.

## Implementation steps (ordered)

1. **Background compiler thread (no behavior change yet).** Spawn it; have the
   interpreter *also* enqueue tasks (in addition to its current inline compile)
   and the thread drain them, but keep using the inline result. Validate the
   queue drains and code is produced off-thread.
2. **Move compilation off the mutator.** Switch the interpreter trigger to
   enqueue-only; the method stays interpreted until the background compile
   publishes. Confirm no first-call stall and no lost compiles under the gauntlet.
3. **Introduce the C1 tier.** Add the fast single-pass-no-opt compile; route the
   manager's `C1`/`C1WithProfiling` recommendations to it, `C2` to the optimized
   path. Honor `on_method_invocation`'s recommended tier instead of discarding
   it (`interpreter.rs:14181`).
4. **Profile handoff C1 → C2.** Have C1 populate the profile structures; have C2
   read them.
5. **Wire `on_backedge` + OSR.** Add back-edge counting and the OSR entry
   (approximate first, precise once deopt/OSR state maps exist).
6. **Tune thresholds** on the gauntlet; expose `CRATONVM_TIER_*` overrides.
7. **Retire** the single fixed-threshold inline path.

## Risks

- **Compile-thread/mutator races**: a method being recompiled while executing,
  or `redefine_class` mid-compile — the `RedefineGate` must cover queued tasks.
- **Code-cache churn**: C1 then C2 doubles compiles per hot method; ensure the
  C1 code is freed when C2 supersedes it (and that no JIT-baked pointer outlives
  it — cf. `MEMORY.md` "bug-24 JIT inline-cache slot UAF").
- **OSR transfer correctness** depends on accurate live-state maps; the
  approximate first cut must be conservative (never enter OSR with a wrong
  locals layout).
- **Throughput regression risk**: if C1 is not actually faster-to-produce than
  today's single inline compile, the extra tier is pure overhead — measure
  compile latency, not just steady-state.
- **Background thread + GC**: the compiler thread must be a safepoint-aware
  participant or excluded from the root set appropriately.

## Effort

L overall; the background-thread + enqueue refactor (steps 1–2) is the bulk and
is independently valuable (removes mutator compile stalls). The real C1 tier
(step 3) is M. Precise OSR (step 5) is gated on `real-frame-deopt.md` state maps;
an approximate OSR is M.
