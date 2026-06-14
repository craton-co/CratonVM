# Group 04 — Embedded-server deployment throughput wall  (OPEN, dominant)

**Status:** OPEN. The single biggest blocker to a green suite.
**Affected:** ~all catalina/coyote embedded-server classes (the 145+ HANG in the
rerun) — `TestSsl`, `TestHostConfigAutomaticDeployment*`, `catalina.startup.*`,
`catalina.connector.*`, etc.

## Symptom

Embedded-server classes deploy a webapp (`tomcat.start()` → `ContextConfig` →
TLD/annotation/jar scanning → servlet init), then serve requests. Under CratonVM
this is grindingly slow (~150s for ONE deploy; HotSpot <1s). With many JUnit
methods per class (e.g. `TestSsl` = 21), a class can't finish within any
practical per-class timeout → classified HANG at the harness's 180s cap.

This is NOT an infinite loop and NOT a TLS bug — it reproduces with JIT off, the
server actually starts/serves/tears-down correctly, just slowly.

## Root cause (quantified)

Pure interpreter throughput on cold, native-heavy deployment code. The measured
hotspot is `update_root_snapshot` (group 03): called on EVERY object-returning
native call — tens of MILLIONS during a single deploy — each scanning an
O(stack-depth ~16-57) frame set. Group 03 removed the lock-contention
amplifier (per-call cost no longer explodes), but the sheer call FREQUENCY ×
O(depth) remains: ~45µs × tens of millions = minutes per deploy. At the default
heap it is further inflated by constant GC (native old-gen spill keeps the heap
full → memory-bandwidth contention).

## Mitigations / next steps

- **Operational (works now):** run with `-Xmx2g` (the harness now passes it) —
  at 2g the GC pressure drops and tests progress (a -Xmx2g run got through all 7
  JSSE `TestSsl` cases, vs grinding at default heap). Server classes ALSO need a
  much larger harness `-TimeoutSec` (≫90/180s) to have a chance to finish.
- **Deferred VM fixes (GC-correctness-critical, dedicated effort):**
  1. Cache the caller-frame portion of `update_root_snapshot`
     (O(depth)→O(1), re-scan only the top frame). FRAGILE: frame mutation is
     scattered across ~15 push/pop sites with frame recycling, so the
     invalidation epoch must be bumped at every site — a missed site = dropped
     root = SEGV. Needs a careful audit.
  2. Reduce per-native publish frequency (only publish when a concurrent
     collector can actually read the snapshot) — needs a collector/mutator
     handshake redesign.
- General interpreter speed (the broader ~20× gap vs HotSpot) is the ceiling.

## Reproduction / measurement

```
cratonvm.exe -Xmx2g --? -cp <cp> org.junit.runner.JUnitCore <serverTestClass>
# env: CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1
#      CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_ROOTSNAP=1
# CWD: apps/tomcat
# read [ROOTSNAP] calls/total_ms/avg_us/avg_frames lines
```
