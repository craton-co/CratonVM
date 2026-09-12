# The tiered compilation manager

**Status:** Shipped (default on; `CRATONVM_BG_COMPILE=0` restores synchronous
on-mutator compilation).

## What it does today

Compilation happens **off-thread by default**, on workers that belong to one
VM's `TieredCompilationManager`. `start_background_compiler`
(`jit/src/tiered.rs`) spawns two lanes of `cratonvm-jit-compiler` threads with
16 MiB stacks: a **C1 lane** for the single-pass tier and a **C2 lane** for the
optimizing tier and every OSR request, so a cheap C1 compile never waits behind
a long C2 or OSR compile. Each worker's `compiler_loop` blocks on its lane's
jit-crate-private `parking_lot` condvar, holding no VM lock, which is what makes
it safe against a stop-the-world pause. The handle lives on the manager, so a
second VM in the same process gets its own workers and dropping a manager joins
them. The VM side is real wiring, not a drain-only stub:
`vm/src/runtime/interpreter/jit_bridge.rs` calls
`tiered::ensure_background_compiler` with `background_compile_task` as the
codegen closure.

Under the default, both invocation triggers enqueue and keep interpreting
until a worker publishes — the static path and the virtual path alike — and
OSR compiles off-thread too. The opt-out restores the historical inline
fixed-threshold and eager-first-call paths verbatim.

Policy lives in `CompilationPolicy::from_env` (`jit/src/tiered.rs`), worker
counts in `tiered::compiler_thread_counts`:

| Knob | Default | Override |
|---|---|---|
| C1 threshold | 500 | `CRATONVM_TIER_C1_THRESHOLD` |
| C2 threshold | 20000 | `CRATONVM_TIER_C2_THRESHOLD` |
| OSR threshold | 10000 | `CRATONVM_TIER_OSR_THRESHOLD` — parsed but not consulted; back-edge OSR is throttled per frame by `CRATONVM_TIER_OSR_BACKEDGE` |
| C2 minimum invocations | 1000 | `CRATONVM_TIER_C2_MIN_INVOCATIONS` |
| tiering enabled | on | `CRATONVM_TIER_ENABLED=0` |
| C1→C2 supersede | on | `CRATONVM_C2_SUPERSEDE=0` |
| C1 lane workers | 1 | `CRATONVM_TIER_C1_THREADS` (clamped to 1..=64) |
| C2 lane workers | max(1, floor(log2(cpus))) | `CRATONVM_TIER_C2_THREADS` (clamped to 1..=64) |

Failure handling is real:

- `MAX_TIER_FAIL_RETRIES = 3` bounds a compile that runs and publishes nothing;
  a policy decline is recorded once as `ineligible` instead.
- A panic in a compile is contained (`tiered::contain_compile_panic`): the
  method is declined, the worker keeps running, and `worker_panic` counts it.
- Deoptimizations are charged HotSpot-style: only actions that throw the body
  away count, per `(reason, bci)` against `PER_BCI_TRAP_LIMIT` and per method
  against `PER_METHOD_TRAP_CUTOFF`, and the counts decay.
- A C2 compile is charged compile-thread CPU time against
  `MAX_C2_COMPILE_TIME_MS`; two successful method-entry overruns demote the
  method to C1.
- OSR denials are per manager, keyed by a loader-aware `MethodKey`, and expire
  when the JIT install epoch moves; a transient OSR compile failure is retried.
- One in-flight slot per method, and `CompilationStats` plus the scheduling
  counters in `jit/src/metrics.rs` make every drop visible.

## What is not built yet

- **Branch profiles are recorded only while a C2 compile is pending.** Branch and
  back-edge recording is off for the whole run by default (`CRATONVM_TIER_PGO`,
  or `CRATONVM_TIER_PGO_ALWAYS`, turns it on). It switches on while a C1→C2
  nomination is outstanding (`CRATONVM_TIER_PGO_C2_WINDOW`, default on), which in
  practice is a few milliseconds per nomination. Receiver and call-site profiles
  are recorded by default (`CRATONVM_TIER_PGO_RECEIVERS=0` opts out). See
  [`profile-guided-inlining.md`](profile-guided-inlining.md).
- `CRATONVM_JIT_FORCE_C2` and `CRATONVM_JIT_C2_FIRST_CALL` remain default-off
  experiments.
- The interpreter's `execute()` tier-up hook has no `CachedBytecodeMethod` in
  hand, so it cannot use the per-call-site settled stamp the dispatch doors use;
  it relies on the 64-call retry stride alone.

## Goal

Turn the existing tiered policy into a real two-tier pipeline: a fast **C1**
(quick, unoptimized, profiling) tier and a profile-driven **C2** (the current
optimizing JIT), with **background compilation threads** draining the
`CompilationQueue`s, and with the **OSR / back-edge** path actually firing
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
  `jit/src/profile.rs` so C2 can use them — this is
  `CompilationTier::C1WithProfiling`. (A second, tiered.rs-local profile with
  `record_branch` / `record_receiver` / `record_type_check` /
  `record_null_check` existed with no VM caller and was deleted.)
- C2 stays the current optimizing backend, now **profile-driven**: it consumes
  the C1-collected profile to bias inlining, branch layout, and (once
  `real-frame-deopt.md` lands) speculative guards.

Practical first cut: make "C1" = the existing single-pass backend with
optimizations off, and "C2" = the existing IR/optimized path. The two
already coexist in `lib.rs::try_compile`; the work is selecting between them by
tier instead of by the current ad-hoc gates.

### 2. Background compilation threads

- Built as two lanes of workers per manager (see "What it does today"). Each
  worker drains its lane → compiles the `CompilationTask` at its `target_tier`
  (and `osr_bci` if set) → publishes the compiled code → records completion.
- The interpreter's trigger blocks enqueue a task at the manager's recommended
  tier and keep interpreting, removing JIT compilation from the mutator's
  critical path.
- Synchronization: each lane's queue is `Mutex`-guarded, and every queued
  request is stamped with the JIT install epoch so a redefinition or code-cache
  flush drops it before it is compiled (`docs/jit/broker-install-epoch.md`).

### 3. OSR / back-edge wiring

- The interpreter's per-frame back-edge schedule (`Frame::should_try_osr`,
  `CRATONVM_TIER_OSR_BACKEDGE`) is the throttle, and it calls
  `tiered_manager.request_osr(key, bci)` directly. The counting
  `on_backedge(key, bci)` door this section originally proposed was built,
  never called, and deleted on 2026-09-12; with it went the only reader of
  `osr_threshold`.
- The C2 lane compiles an **OSR entry**: a version of the method entered at
  `osr_bci` with the live interpreter locals/stack transferred into the compiled
  frame. This is the mirror image of deopt (frame *materialization* vs frame
  *capture*) and shares the same per-bci state map machinery from
  `real-frame-deopt.md`.
- **Threshold tuning**: the invocation threshold (default 500) has no loop
  awareness; the per-frame back-edge schedule is what compiles a long-running
  loop in a rarely-*invoked* method. Tune `CRATONVM_TIER_OSR_BACKEDGE` and
  `c1_threshold` (500) against the app gauntlet — HotSpot's defaults are a
  reference but CratonVM's compile cost differs.

## Risks

- **Compile-thread/mutator races**: a method being recompiled while executing,
  or `redefine_class` mid-compile — the install-epoch gate covers queued tasks,
  and `JitCache::put`'s flush barrier covers in-flight ones.
- **Code-cache churn**: C1 then C2 doubles compiles per hot method; ensure the
  C1 code is freed when C2 supersedes it (and that no JIT-baked pointer outlives
  it — cf. `MEMORY.md` "bug-24 JIT inline-cache slot UAF").
- **Concurrent C2 workers**: with more than one C2 worker, two different methods
  compile at once on the optimizing backend. The one-slot-per-method rule keeps
  a method from being compiled twice concurrently, but any compile-global state
  the backend assumes is single-threaded is exposed; `CRATONVM_TIER_C2_THREADS=1`
  restores one optimizing compile at a time.
- **OSR transfer correctness** depends on accurate live-state maps; the
  approximate first cut must be conservative (never enter OSR with a wrong
  locals layout).
- **Throughput regression risk**: if C1 is not actually faster-to-produce than
  today's single inline compile, the extra tier is pure overhead — measure
  compile latency, not just steady-state.
- **Background threads + GC**: the compiler threads must be safepoint-aware
  participants or excluded from the root set appropriately.
