# Workstream 1 — JIT/interpreter throughput (kafka suite watchdog trips)

This is the root-cause analysis behind bug-01 and the "slow" side of bug-05/06:
many kafka-clients packages *complete* but exceed the 120 s default watchdog because
CratonVM is far slower than HotSpot on these workloads. It is a **performance**
workstream, not a discrete crash.

## Measured findings (`org.apache.kafka.common.config`, the cleanest case)

| Configuration | Wall time |
|---|---|
| HotSpot 25 tiered (default) | **~0.8 s** (compiles 2845 methods, but **async/background**) |
| HotSpot 25 `-Xint` (interpreter only) | **~1.3 s** |
| CratonVM `--nojit` (pure interpreter) | **9.5 s** |
| CratonVM JIT-on (default) | **227 s** |

Two separate gaps:
- **JIT inversion (the catastrophe):** CratonVM JIT-on is **~24× *slower* than
  CratonVM's own interpreter** — the opposite of what JIT should do. HotSpot's JIT'd
  code is *faster* than its interpreter; CratonVM's is *slower* for call-heavy code.
- **Interpreter gap (secondary):** CratonVM's interpreter (9.5 s) is ~7× slower than
  HotSpot's (`-Xint` 1.3 s).

HotSpot stays fast because it is **interpreter-first with asynchronous (background)
compilation**: short-lived code is interpreted, hot methods are compiled off-thread
and swapped in, so the JIT never *slows* a workload. CratonVM compiles eagerly and
its JIT'd call-heavy code executes slower than interpreting it → the 24× penalty.
The HotSpot-aligned fix is to not JIT code where the JIT'd version isn't faster
(higher/smarter threshold or async compile), validated against the compute benchmarks.

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

## Attempt (a): make the dispatch helper polymorphic — investigated, reverted

A polymorphic inline cache is in fact **already implemented**: `JitPICSlot` (3 entries),
an inline 3-way class cascade emitted at each `invokevirtual`/`invokeinterface` site
(`jit/src/x64.rs`), MIC→PIC→megamorphic promotion thresholds, and helper-side PIC
population. The remaining gap was that `jit_invoke_virtual_mic`, on a MIC miss, went
straight to full `invoke_or_native` resolution **without consulting the PIC's 3
entries** — so for `needs_context==false` callees (which bypass the inline cascade)
and cascade spillover, the helper was effectively monomorphic.

I added a PIC `lookup` in the helper's miss path (dispatch directly on a PIC hit).
Measured result: **net-negative / no help** —
- `common.config` JIT-on did **not** improve (still >280 s): its discovery dispatch is
  genuinely **>3-way megamorphic**, so a 3-entry PIC thrashes (LFU evicts every round)
  and the added per-call `lookup` scan is pure overhead with ~no hits.
- so the change was **reverted** rather than shipped (it could only hurt
  megamorphic-heavy paths and didn't help the target). Correctness was unaffected
  (polymorphic dispatch probe stayed correct).

**Conclusion:** the config case is beyond a 3-entry PIC. The durable levers remain a
**larger / profile-driven PIC**, **eliminating per-call marshalling** (Vec alloc +
descriptor re-parse in the helper), and **inlining interpreter intrinsics into JIT'd
code** — each a substantial JIT-quality change, validated against the bench suite.

> **Separate observation (not from this work):** `common.serialization` JIT-on
> *completed* right after dev `3ccd0bef` earlier this session but now **times out on a
> clean dev binary** (no local changes). A dev commit merged since then appears to
> have regressed JIT-on discovery throughput/stability — worth a dedicated bisect.

## Validated lever: env-configurable JIT threshold (`CRATONVM_JIT_THRESHOLD`)

The JIT warmup threshold (hardcoded `500` invocations in two places — the interpreter
upgrade gate and the dispatch-helper gate) is now read from `CRATONVM_JIT_THRESHOLD`
(default **500**, so default behaviour is unchanged). Raising it keeps medium-hot
call-heavy code interpreted instead of paying CratonVM's slower JIT'd dispatch.

Measured (in-JVM ms / wall; machine under some concurrent load, hence noise):

| Workload | thr=500 (default) | thr=5000 | thr=50000 |
|---|---|---|---|
| **bintrees18** (compute benchmark) | 10418 ms | — | **10007 ms** (no regression) |
| kafka `common.config` (call-heavy) | 227 s | ~112 s | ~120 s |

**Key validation:** the compute benchmark is **unaffected** by a 100× higher
threshold — its hot recursion crosses any threshold within the first few k calls and
still compiles, so steady-state JIT throughput is identical. Meanwhile kafka's
call-heavy JIT penalty roughly halves.

**Limits (honest):** config still plateaus ~120 s — it has genuinely-hot framework
methods (>50k calls) whose JIT'd code is *itself* slower than interpreting them, which
no threshold can fix (those methods cross any threshold and compile). So this knob is
a **partial mitigation**, not a cure; the cure is the JIT codegen-quality work above.

**Default left at 500** (zero behavioural change / zero regression risk): only
`bintrees18` was validated here, not the full app suite. Flipping the default to a
higher value should follow a full bench-suite + app-smoke run; the knob lets that be
trialed without a rebuild.

## Practical status
- The whole kafka-clients unit suite runs correctly under **`--nojit`** (the three
  genuine correctness bugs — bug-02/03/04 — are fixed). JIT-on is a throughput/quality
  gap, scoped above.
- A contributing per-call overhead (the OOB-guard `String` alloc + `warn!` flood) was
  fixed separately (see bug-06).
