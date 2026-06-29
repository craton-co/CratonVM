# TC0622 `TestCoyoteOutputStream` NOSUMMARY — JIT startup-latency tax, NOT an async-I/O bug

> **One-line:** The `testNonBlockingWrite*` (Servlet 3.1 non-blocking `WriteListener`)
> tests are **correct** — every variant passes in isolation. The whole class
> reports **NOSUMMARY** in the suite only because each test re-creates a Tomcat
> instance and `tomcat.start()` takes ~40–50s **with the JIT enabled** (vs ~14s
> `--nojit`); ×13 test methods that blows past the harness's 180s/class timeout,
> so the process is killed mid-run before JUnit prints a summary.

**Status:** Non-blocking tests PASS (no defect). Root cause = general
JIT-startup-throughput ("the throughput wall"). **Date:** 2026-06-23.

## RESOLUTION (2026-06-23) — profiled and the dominant cost FIXED

A native sampling profile (samply/ETW, full-debuginfo `profsym` build, CPU-time
weighted) of `tomcat.start()` localized the cost to **one function consuming
~66% of all CPU**: `cratonvm_jit::lookup_jit_code_range`.

Chain: `update_root_snapshot` (per native call) → `scan_active_jit_frames` →
once any method compiles (`jit_code_range_count() > 0`; precise maps default-on)
→ `native_stack_has_jit_frame` scans up to ~1M native-stack words, and **each
word locked a global `std::sync::Mutex` + linear-scanned the range Vec** via
`lookup_jit_code_range`. JIT-gated → `--nojit` never registers ranges → ~5×
faster. This is also why neither the counter-sharding nor the high-threshold
experiments helped: the cost was the per-word Mutex in the GC root scan, not the
counters or compiles.

**Fix** (branch `fix/jit-startup-rangescan`, commit e601c4ba): snapshot the
small disjoint range set ONCE per scan into a reusable thread-local buffer (one
lock, released before the word loop) and binary-search each word lock-free —
behavior-identical. Opt-out `CRATONVM_JIT_RANGE_SCAN_LEGACY=1`.

Measured: `start()` ~48s → ~22s (~11s on one profiled run); re-profile shows the
function fell from 66% → ~25% self-time and total CPU dropped ~62%. bt16
GC-stress = 14985902 (==HotSpot) on both paths.

**Follow-up #1 (done — small safe win):** after the Mutex removal,
`native_stack_has_jit_frame` was still ~25% self-time. Added an **address-envelope
prefilter** (commit 21bfd096): JIT code occupies a narrow address band, so a
stack word outside `[min_start, max_end)` is rejected with one compare before the
binary search (strict superset → identical result). `start()` ~22s → ~20s,
bt16==HotSpot. The gain is small because the residual is **memory-bandwidth-
bound** — the raw per-native-call read of the stack band, not per-word compute.

**Why it can't be cut further cheaply:** the per-native-call scan is
load-bearing — the authoritative cross-thread-STW snapshot is rebuilt at the
safepoint (`safepoint_check`, interpreter.rs:1880), so the per-call scans exist
to cover threads that **block in native code before reaching a safepoint** (their
last per-call snapshot is the only view the collector gets). Scanning less often
/ less of the band risks dropping a live root for such a thread → the exact
heap-corruption class this A5 net prevents. The clean elimination is **precise
JIT stack maps** (so the conservative band scan is unnecessary) — a separate
epic.

## OUTCOME — class now PASSES

With the RAF fd-registry fix (`testWriteWithByteBuffer` → 200) **and** the JIT
startup fix, the full class now runs green: `run-suite.ps1` reports
**`PASS 56s — OK (14 tests)`** (`Tag=tc0622bump`, the fixed binary). The 56s
total (vs ~20s for one isolated cold method) is warm-JVM amortization — after the
first test the startup methods are already compiled, so the remaining 13 starts
are cheap. The JIT fix alone got it under even the default 90s/class timeout.

As a general safety net for other heavy embedded-server classes, `run-suite.ps1`
now supports a per-class timeout override (CSV `class,seconds`, default
`.tooling/class-timeouts.csv`); unlisted classes keep the small default so
genuine hangs still die fast. Seeded with `TestCoyoteOutputStream,720`.
(`.tooling/` is gitignored local harness — not a repo/CI change.)

## Tooling note (no admin / cross-thread)

ETW kernel sampling normally needs admin; this account could run `samply record`
non-elevated. CratonVM's cross-thread `Thread.getStackTrace()` returns empty for
a running thread, so a Java-level in-process sampler does NOT work — use the
native profiler. `samply --unstable-presymbolicate` only resolved PUBLIC symbols
(needs `debug = 2`, not `line-tables-only`); the `[profile.profsym]` profile
(full debuginfo) plus a tiny `pdb-addr2line` RVA→name tool
(`apps/tomcat/.tooling/pdbresolve`) gave real Rust names. The libffi-sys
"Pre-process ASM" fresh-build panic needs the `$env:INCLUDE` libffi-dirs fix
before vcvars (see reference_libffi_sys_build_fix).

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
