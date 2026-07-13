# Silent 1200s hangs with no diagnostic signature (3 classes)

**Status:** PARTIALLY FIXED (3 root causes found and fixed 2026-07-13;
branch `fix/silent-hang-no-signature-20260713`), residual throughput issue
OPEN. **Severity:** medium (was high/indefinite-hang; now bounded-but-slow
in the affected code paths). **HotSpot:** PASS on all 3 (fresh-verified).

## Summary

Three classes HANG at the full 1200s timeout without printing any
error/warning in stdout or stderr beyond normal startup:

- `org.apache.catalina.startup.TestContextConfig` — log stops right after
  `INFO [org.apache.catalina.startup.ContextConfig] No global web.xml
  found`.
- `org.apache.catalina.connector.TestResponsePerformance` — log stops
  right after `INFO [...] Starting test case [testToAbsolutePerformance]`.
- `org.apache.jasper.compiler.TestValidator` — log stops shortly after
  `JUnit version 4.13.2`.

**Heap-sizing false positive theory REFUTED**: all 3 still hang
identically at `-Xmx12g` (previous hypothesis, from a related Linux OOM
finding, ruled out).

## Root causes found + fixed (2026-07-13)

Three independent, silent-by-design bugs, each causing the interpreter to
either waste enormous cumulative time re-attempting doomed work forever,
or pay an unconditionally expensive check on every single instruction.
None produce a warning because they are "successfully" bailing/skipping
each time — just far too often.

1. **JIT compile-bail not permanently recorded for `ldc`/`ldc2_w`
   constants** (`jit/src/lib.rs::try_compile_inner`). Resolving a
   `ldc`/`ldc2_w` operand to a String/Class/MethodHandle (or non-
   Long/Double) constant returns `None` via `?` WITHOUT setting
   `backend_attempted`, so `mark_jit_bail_listed` never fires. A hot
   method containing such a constant (confirmed repro: `TesterRequest
   .getRequestURI() { return "/level1/level2/foo.html"; }`, called from
   `TestResponsePerformance`'s 1,000,000-iteration loop) re-ran the ENTIRE
   compile gauntlet (skip-list check + native-shadow hierarchy walk +
   bytecode scan) every ~2000 invocations, forever — confirmed via
   `CRATONVM_DBG_JITC=1`: 20 `compile-bail ... backend_attempted=false`
   lines for the same method in one 60s window pre-fix, 1 post-fix.
   **Fixed**: mark permanently bailed, matching the existing RBC.4/RBC.6
   convention already used for scan-reject/athrow-with-handler.

2. **OSR re-enqueue has no failure cap** (`jit/src/tiered.rs
   ::request_osr`). Unlike `should_compile`/`request_c2_upgrade`, this
   function never checked `tier_fail_count >= MAX_TIER_FAIL_RETRIES`. An
   OSR artifact compile that keeps returning `published=false` leaves
   `current_tier` stuck below C2 and `queued_for_compilation` cleared
   after each failure, so the very next hot back-edge re-enqueues a fresh
   OSR task — forever. Confirmed via `CRATONVM_DBG_JITC=1`:
   `TestResponsePerformance.doHomebrew`'s `bg-compile ... osr_bci=7`
   repeated 5 times in 60s pre-fix (no cap); exactly 3 times post-fix
   (matches `MAX_TIER_FAIL_RETRIES`), then stops.
   **Fixed**: added the same cap check `request_c2_upgrade` already has.

3. **Uncached env-var syscall on every bytecode instruction**
   (`vm/src/runtime/interpreter.rs`). Five leftover "ALV5th GC
   investigation (temp probe, CRATONVM_DBG_DESCTRACE)" debug blocks from a
   completed, unrelated investigation — one **unconditionally at the top
   of `execute_instruction`, fired for every single bytecode dispatched
   by the whole VM**, three more inside the `Getfield`/`Putfield` arms
   (the two hottest OO opcodes), one inside JIT NPE handling. Each did a
   raw uncached `std::env::var_os("CRATONVM_DBG_DESCTRACE")` (a real OS
   syscall), unlike every other `CRATONVM_DBG_*` flag in this file which
   uses the cached `env_cache` module.
   **Fixed**: removed all 5 (dead debug code, no ongoing purpose).

Verified fixes are non-regressive: a 30-class sample regression run
showed 5 new-looking hangs, but each traced to transient host contention
(4-way parallel + a 60s timeout on an already-overloaded host; e.g.
`TestCompositeELResolver` HANG in that batch, PASS in 51.3s when re-run
in isolation with no siblings).

## Residual (still OPEN): interpreter throughput

Even with all 3 fixes, none of the 3 classes reliably completes within
the suite's 300–1200s timeout on this dev box. Root-caused (not just
suspected) via a scaled standalone microbenchmark directly exercising
`Response.toAbsolute()` (same code `TestResponsePerformance` calls in its
hot loop):

```
n=1000    elapsed=967ms    (~0.97ms/call)
n=10000   elapsed=5693ms   (~0.57ms/call)
n=100000  elapsed=60475ms  (~0.60ms/call)
```

Roughly **linear** (not exponential/quadratic — ruling out an unbounded-
growth bug), but the constant factor (~0.6ms per call to a method that
should take low single-digit microseconds) is ~300-600x too slow for
`TestResponsePerformance`'s up to 6 full 1,000,000-iteration passes (1
warmup + 5 measurement rounds × `doHomebrew`, plus matching `doUri`
rounds) to fit in any reasonable timeout. Repeated stack sampling
(`cdb -p <pid> -c ".lines; ~*kn; qd"`, several rounds) during a live run
showed genuinely diverse activity across the interpreter's normal
dispatch machinery (cached-invoke resolution, native-override special-
casing in `force_native_over_real_jdk_bytecode`/
`should_force_registered_native_over_bytecode`, redefine-gate staleness
checks, field retargeting) rather than one single dominant hot spot —
process CPU-time accounting confirmed the thread is genuinely computing
throughout (not blocked/deadlocked: ~975s user-mode CPU consumed over
~1100s wall-clock in one sample). This reads as accumulated per-call
interpreter dispatch overhead across many safety/compatibility checks
compounding on method-call-heavy code, not a single discrete bug — likely
a larger, separate interpreter-throughput investigation (in the vein of
the existing `reference_hot_op_helperization_trap` pattern), not a quick
fix.

`TestContextConfig` additionally hit a SEPARATE, apparently-intermittent
bug in one direct (non-suite-harness) run: all 8 sub-tests failed fast
(16.7s total, not a hang) with `LifecycleException: A child container
failed during start` / `Failed to start component
[org.apache.catalina.webresources.StandardRoot@...]`. Not yet
root-caused — worth a dedicated investigation; may or may not be related
to the throughput residual above (a component that fails fast instead of
running the expensive path would trivially "fix" the hang for the wrong
reason).

**Confounding factor for any future measurement on this specific host**:
an active cryptominer infection was confirmed present throughout this
investigation (`unsecapp.exe` process masquerades with 5000+ accumulated
CPU-seconds, `RecoveryManager`/`RecoveryHosts` scheduled tasks — see
memory `reference_windows_box_cryptominer_infection_20260710`). Even
P-core-pinned + high-priority runs still hung, so this is not purely a
"noisy neighbor" explanation for the residual — but it does explain why
a clean pass and a 1200s+ hang were both observed for the identical
binary/test within the same session with no code change in between.
Re-verify on a clean host (or after remediation) before fully trusting
absolute wall-clock pass/fail for the residual throughput issue.

## Reproduction

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName silenthang `
  -Start <idx> -Count 1 -TimeoutSec 300 -Parallel 1
# org.apache.catalina.startup.TestContextConfig
# org.apache.catalina.connector.TestResponsePerformance
# org.apache.jasper.compiler.TestValidator
```

JIT compile-bail tracing (for the fixed bugs, to confirm regression-free
on a future dev tip): `CRATONVM_DBG_JITC=1`, then grep the class's stderr
for `bg-compile`/`compile-bail`/`osr_bci` — a healthy run shows each
uncompilable method bail at most `MAX_TIER_FAIL_RETRIES` (3) times total,
never repeating indefinitely.

## Recommendation for continuing this residual

1. Re-verify the 3 classes' actual completion time on a clean (idle,
   unmined) host — the fixes above are confirmed correct in isolation but
   an end-to-end "PASS within 1200s" verdict needs a clean measurement.
2. If still too slow: this is now a throughput problem, not a logic bug.
   Consider whether `TestResponsePerformance`'s `ITERATIONS = 1_000_000`
   assumption is fundamentally incompatible with an interpreter this far
   from HotSpot's steady-state throughput, vs. profiling
   `force_native_over_real_jdk_bytecode` (a ~1400-line sequential
   string-comparison special-case gauntlet run on every non-JIT'd
   invoke) and similar always-checked special-case functions for
   optimization opportunity (e.g., a fast-path dispatch keyed by class
   name hash before the sequential checks).
3. Root-cause the `StandardRoot` component-start failure separately (own
   investigation; only seen once so far, may be flaky/environmental too).
