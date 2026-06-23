# TC0622 `TestCoyoteOutputStream` NOSUMMARY — JIT startup-latency tax, NOT an async-I/O bug

> **One-line:** The `testNonBlockingWrite*` (Servlet 3.1 non-blocking `WriteListener`)
> tests are **correct** — every variant passes in isolation. The whole class
> reports **NOSUMMARY** in the suite only because each test re-creates a Tomcat
> instance and `tomcat.start()` takes ~40–50s **with the JIT enabled** (vs ~14s
> `--nojit`); ×13 test methods that blows past the harness's 180s/class timeout,
> so the process is killed mid-run before JUnit prints a summary.

**Status:** Non-blocking tests PASS (no defect). Root cause = general
JIT-startup-throughput ("the throughput wall"), which is a separate, deep
JIT-perf effort. **Date:** 2026-06-23.

## Evidence

Isolation runs (single test method via a `Request.method` launcher, worktree
binary off `dev`):

| What | Result |
|---|---|
| `testNonBlockingWriteNoneBlockingWriteNoneContainerThread` | PASS, **41.6s** |
| `testNonBlockingWriteOnceBlockingWriteOnceNonContainerThread` | PASS, 39.0s |
| `testNonBlockingWriteTwiceBlockingWriteOnceContainerThread` | PASS, 65.3s |
| `testWriteWithByteBuffer` (blocking, single request) | PASS, 66.1s |
| `testWriteAfterBodyComplete` (blocking) | PASS, 44.4s |

The **blocking** single-request tests cost the same ~40–66s as the non-blocking
ones, so the latency is **not** non-blocking-specific.

Phase timing of a trivial embedded-Tomcat start/serve/stop (`StartStopTiming`):

```
setup=176ms  start=48,632ms  request=451ms  stop=3,072ms   (JIT default)
setup=116ms  start=10,327ms  request=140ms  stop=2,766ms   (--nojit)
```

`tomcat.start()` is the entire cost, and the JIT roughly **5×'s** it.
`--nojit` reliably reproduces ~14s start; JIT default ~40–53s (high variance).

## What it is NOT (each disproved by measurement)

- **Not compile volume.** `CRATONVM_DBG_JITC` shows only ~8 compiles during a
  full startup. With `CRATONVM_JIT_THRESHOLD=100000000` (nothing crosses the
  invocation threshold) only **2** compiles happen, yet `start()` is still
  **~32s** — ~19s above `--nojit`. So the overhead is **not** the act of
  compiling.
- **Not invocation-counter lock contention.** Sharding the single global
  `ProfileStore::invocation_counts` `Mutex<HashMap>` into `PROFILE_SHARDS`
  mutexes (behavior-preserving) produced **no measurable improvement**
  (44/41/39s vs 48s baseline). Change was reverted — no unsubstantiated churn to
  the hot path.
- **Not a busy-spinning background compiler.** `tiered::compiler_loop` blocks on
  its `Condvar` (`core.wake.wait`) when the queue is empty; it does not spin.
- **Not `CRATONVM_BG_COMPILE`.** `=0` (the pre-Step-7 inline path) is *also*
  ~53s, so this is **longstanding**, not the recent default-on background-compile
  flip.

## What it is (localized but not yet root-caused to a line)

`--nojit` only sets `CRATONVM_DISABLE_JIT=1`. The `disable_jit()` gates live at:
`interpreter.rs:3186` (the `execute()` first-call JIT block) and the three
compile entry points (`compile_osr_artifact` 18522, callee/upgrade 19704/20641).
The per-call inline-dispatch bookkeeping in the `CachedInvokeTarget::Bytecode`
arm (`interpreter.rs:18160` — `jit_cache.read().get()` re-hashing
class+method+descriptor, then `increment_invocation`) is **not** gated by
`disable_jit`, so it runs in both modes and is not the differentiator.

Removing invocation-triggered compiles (high threshold) recovers ~12s of the
~38s gap; the residual ~19–24s persists with ~2 compiles and is **per-call JIT
dispatch overhead behind the `disable_jit` gate** that static reading did not
localize to a single line. **Localizing it needs a native sampling profiler**
(CratonVM's cross-thread `Thread.getStackTrace()` returns empty for a running
thread, so a Java-level in-process sampler does not work here).

## Impact & recommendation

This startup tax hits every heavy-startup app in the gauntlet (Tomcat, Spring,
Wildfly, …) — it is the "Group-04 throughput wall" referenced elsewhere. It is a
dedicated, profiler-driven JIT-perf effort that must be **gauntlet-soaked**
before any default change (the JIT dispatch is the hottest, most soak-sensitive
path in the VM).

Interim, to keep these heavy-startup test *classes* from false-NOSUMMARY in the
Tomcat suite: run them with a higher per-class `-TimeoutSec` in
`apps/tomcat/.tooling/run-suite.ps1`, or with `--nojit`. No VM change is
warranted purely for the harness timeout.

## Reproduce

```powershell
# phase timing — embedded Tomcat start/serve/stop, JIT vs nojit
$exe = "<cratonvm.exe>"; $cp = (gc apps\tomcat\.tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1; $env:CRATONVM_REAL_AQS=1; $env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
# StartStopTiming.java: new Tomcat(); addContext/addServlet; time start(); 1 request; time stop()
& $exe          -cp "$rep;$cp" StartStopTiming   # ~48s start
& $exe --nojit  -cp "$rep;$cp" StartStopTiming   # ~14s start
```
