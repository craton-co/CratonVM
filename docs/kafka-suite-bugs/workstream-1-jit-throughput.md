# Workstream 1 — JIT/interpreter throughput (kafka suite watchdog trips)

This is the root-cause analysis behind bug-01 and the "slow" side of bug-05/06:
many kafka-clients packages *complete* but exceed the 120 s default watchdog because
CratonVM is far slower than HotSpot on these workloads. It is a **performance**
workstream, not a discrete crash.

## Measured findings (`org.apache.kafka.common.config`, the cleanest case)

| Configuration | Wall time |
|---|---|
| HotSpot 25 | ~9 s |
| CratonVM `--nojit` (pure interpreter) | **9.5 s** |
| CratonVM JIT-on (default) | **227 s** |

So **JIT-on is ~24× *slower* than the interpreter** for this workload — the opposite
of what JIT should do.

### What it is *not* (ruled out by instrumentation)
- **Not compile-time:** with `CRATONVM_DBG_JITC` timing, **0** of the 101 compiles
  took >5 ms. Compilation is fast.
- **Not recompile churn:** 101 first-compiles, **0** re-compiles, **0** OSR.
- **Not deopt thrashing:** `CRATONVM_DBG_DEOPT` shows **0** deopt events.
- **Not an infinite loop:** with the watchdog off the run completes (227 s).

### What it is
The cost is in **executing the JIT-compiled code**, specifically virtual/interface
dispatch out of JIT'd call-heavy framework code (JUnit `ReflectionUtils` /
`AnnotationUtils` / `Preconditions`, JDK reflection, `HashMap`, `Optional`, streams):

1. `jit_invoke_virtual_mic` has a **monomorphic** inline cache (MIC) — fast only when
   a call site sees a single receiver class. Framework dispatch is heavily
   **megamorphic**; those sites **miss the MIC on every call** and fall back to a full
   `class_manager` lookup + method resolution per call.
2. Every virtual call (hit or miss) allocates a `Vec` for the argument values and
   re-parses the method descriptor before the MIC check — per-call marshalling
   overhead the interpreter doesn't pay.
3. The interpreter, by contrast, has **native intrinsics** and a more forgiving
   polymorphic dispatch for exactly these hot JDK methods, so it's faster here.

Net: JIT'd call-heavy code is slower than interpreted call-heavy code.

## Why this isn't a one-line fix (and what the real fixes are)
- The compile **threshold** (500 invocations) and global JIT policy are tuned for the
  project's compute benchmarks (bintrees, crypto). Raising it would help these
  short-lived call-heavy workloads but risks regressing those benchmarks — it needs
  validation against the full bench suite, not a blind change.
- The durable fixes are JIT-quality work:
  - a **polymorphic** (not just monomorphic) inline cache for megamorphic sites;
  - eliminate the per-call `Vec` alloc + descriptor re-parse in the dispatch helper
    (stack buffer / cache the parsed descriptor);
  - **inline the interpreter intrinsics** into JIT'd code so JIT'd calls to
    `HashMap`/reflection hot methods get the same fast path the interpreter uses;
  - optionally, **async/background compilation** so even a wrong compile decision
    never stalls the running thread (HotSpot's model).

## Practical status
- The whole kafka-clients unit suite runs correctly under **`--nojit`** (the three
  genuine correctness bugs — bug-02/03/04 — are fixed). JIT-on is a throughput/quality
  gap, scoped above.
- A contributing per-call overhead (the OOB-guard `String` alloc + `warn!` flood) was
  fixed separately (see bug-06).
