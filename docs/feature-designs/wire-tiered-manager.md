# The tiered compilation manager

**Status:** Shipped (default on; `CRATONVM_BG_COMPILE=0` restores synchronous
on-mutator compilation).

## What it does today

Compilation happens **off-thread by default**. `start_background_compiler`
(`jit/src/tiered.rs`) spawns a named `cratonvm-jit-compiler` thread with a
16 MiB stack; `compiler_loop` blocks on a jit-crate-private `parking_lot`
condvar, holding no VM lock, which is what makes it safe against a
stop-the-world pause. The VM side is real wiring, not a drain-only stub:
`vm/src/runtime/interpreter/jit_bridge.rs` calls
`tiered::ensure_background_compiler` with `background_compile_task` as the
codegen closure.

Under the default, both invocation triggers enqueue and keep interpreting
until the worker publishes — the static path and the virtual path alike — and
OSR compiles off-thread too. The opt-out restores the historical inline
fixed-threshold and eager-first-call paths verbatim.

Policy lives in `CompilationPolicy::from_env` (`jit/src/tiered.rs`):

| Knob | Default | Override |
|---|---|---|
| C1 threshold | 500 | `CRATONVM_TIER_C1_THRESHOLD` |
| C2 threshold | 20000 | `CRATONVM_TIER_C2_THRESHOLD` |
| OSR threshold | 10000 | `CRATONVM_TIER_OSR_THRESHOLD` |
| C2 minimum invocations | 1000 | `CRATONVM_TIER_C2_MIN_INVOCATIONS` |
| tiering enabled | on | `CRATONVM_TIER_ENABLED=0` |
| C1→C2 supersede | on | `CRATONVM_C2_SUPERSEDE=0` |

Failure handling is real: `MAX_TIER_FAIL_RETRIES = 3`, a dynamic
`osr_deny_list`, and `CompilationStats` counters.

## What is not built yet

- **Profile recording is default-off** (`CRATONVM_TIER_PGO`), so the
  evidence the C2 tier could speculate on is not being collected in a default
  run. See [`profile-guided-inlining.md`](profile-guided-inlining.md).
- `CRATONVM_JIT_FORCE_C2` and `CRATONVM_JIT_C2_FIRST_CALL` remain default-off
  experiments.

## Goal

Turn the existing tiered policy into a real two-tier pipeline: a fast **C1**
(quick, unoptimized, profiling) tier and a profile-driven **C2** (the current
optimizing JIT), with a **background compilation thread** draining the existing
`CompilationQueue`, and with the **OSR / back-edge** path actually firing
on-stack replacement for hot loops.

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

