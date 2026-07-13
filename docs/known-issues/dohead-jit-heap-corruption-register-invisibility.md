# Tomcat `TestHttpServletDoHead*` — JIT-only young-gen heap corruption / SIGSEGV

**Status (2026-07-13): RESTORED to known-issues.** This doc was previously
retired to `docs/internal` under a `-FIXED` filename because its two
documented root causes were genuinely fixed and merged to `dev`:

- **Root cause #1** (young from-space walk-desync / GC corruption, "FATAL
  LAYER" below) — commit `928cc5b3`, confirmed present in current `dev`
  history (`git merge-base --is-ancestor 928cc5b3 origin/dev` succeeds).
- **Root cause #2** (`StreamEncoder` eager-flush breaking the commit
  threshold) — commit `1773d3df2` ("fix: buffer StreamEncoder writes to
  match real HotSpot flush granularity"), also confirmed present in `dev`.
  See `dohead-streamencoder-eager-flush-commit-threshold.md` (also restored
  from `docs/internal/fixed-suite-bugs/`).

**2026-07-13 cross-reference note:** the `String.setOption` signature below
was hypothesized (in `largeclienthello-string-size-nosuchmethod.md`, now
fixed and moved to `docs/internal/tomcat-08-07/`) to share a root cause with
an unrelated `NoSuchMethodError: java/lang/String.size()I` in
`ClassLoaderLogManager.resetLoggers()`. That hypothesis was investigated and
**refuted**: the `resetLoggers` bug was a deterministic real-vs-synthetic
`java.util.logging.Logger` field-slot collision (fixed — see that doc's
"Refuted hypothesis" section for the full writeup). The `Socket.setSoTimeout`
family here is a *different*, **non-deterministic** bug — reruns under
identical conditions vary between `String.setOption` NoSuchMethodError,
`socketLock`-is-null NPE, and `SocketException: Socket is closed` — which
points at the register-invisible-root / stale-reference-reuse family this
doc already documents below, not a shared vtable-dispatch defect. Don't
re-open the "shared root cause with the Logger bug" angle without new
evidence.

**However, the whole DoHead family still does not pass today.** A fresh
full-suite rerun on `dev` (commit `080e79256`, 2026-07-12, real JDK, JIT on,
1200s timeout) shows all ~19
`jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite*` classes still
FAIL or HANG — none PASS. Spot-checking
`TestHttpServletDoHeadInvalidWrite1024ValidWrite512` (the exact class the
StreamEncoder fix validated as `OK (288 tests)` at the time) now shows
**152 of 288 failures**, and the failure signatures are **neither of the
two documented root causes**:

```
java.lang.NullPointerException: Cannot enter synchronized block because "this.socketLock" is null
	at java.net.Socket.getImpl(Socket.java:493)
	at java.net.Socket.setSoTimeout(Socket.java:1278)
	at org.apache.coyote.http2.Http2TestBase.openClientConnection(Http2TestBase.java:700)

java.net.SocketException: Socket is closed
	at java.net.SocketException.<init>(SocketException.java:47)
	at java.net.Socket.setSoTimeout(Socket.java:1275)

java.lang.NoSuchMethodError: java/lang/String.setOption(ILjava/lang/Object;)V
	at java.net.Socket.setSoTimeout(Socket.java:1278)
```

The same `socketLock`-NPE / `Socket is closed` pair shows up consistently
across other DoHead siblings too
(`TestHttpServletDoHeadInvalidWrite1023ValidWrite0`,
`TestHttpServletDoHeadInvalidWrite0ValidWrite1023`, spot-checked). This is
**not** the same thing as the existing tentative
`dohead1023-http2-index0-socketexception-likely-host-contention.md` doc
(`docs/internal/`) — that doc saw only 2/288 flaky failures in one class
and explicitly couldn't reproduce them reliably, hypothesizing shared-host
CPU contention. What's described here is much larger in scope (152/288,
consistent across multiple sibling classes) and includes a **deterministic,
non-timing-sensitive** signature (`NoSuchMethodError:
java/lang/String.setOption(ILjava/lang/Object;)V` — `Socket.setSoTimeout`
dispatching into a `String` method surface looks like a native-call/vtable
resolution bug, not a flaky timing artifact). Likely a genuine, currently
open, undocumented third root cause in `Socket`/HTTP2 test-connection
handling — worth its own investigation before assuming it's the same
low-confidence host-contention theory.

## Original write-up follows (root causes #1 and #2, both now fixed on dev)

**Note (2026-07-06):** at a long-enough timeout (1200s) that the crash/hang
this doc describes doesn't mask it, `TestHttpServletDoHeadInvalidWrite1024ValidWrite512`
completes and shows 16 deterministic, non-flaky test failures — a completely
SEPARATE root cause (a `StreamEncoder` real-mode shim eager-flush bug that
broke `NoBodyOutputStream.checkCommit()`'s byte-count-based commit
threshold), confirmed unrelated by log-line evidence (the one GC corruption
WARN in the baseline run falls between two unrelated `testDoHeadHttp2`
parameterizations, nowhere near the 16 failing cases). See
`dohead-streamencoder-eager-flush-commit-threshold.md` for that fix. Do not
conflate the two when triaging future DoHead failures.

Status: **FATAL LAYER FIXED** on branch `fix/dohead-sweep-freelist` (commit
`928cc5b3`, 2026-07-02): the crash was NOT (only) the register-invisible root —
that is Layer 1, survivable. The FATAL layer was the **young from-space
walk-desync family**: an unlisted zeroed span (freed-then-reused slot whose new
owner's header a stale register-held reference clobbered back to zero, or
freed-but-unlisted residue) was strided as phantom 40-byte "objects" and each
phantom RE-FREED; spans are 40+16n bytes so the final phantom window crossed
into the next LIVE object's header — zeroing it and minting a free block inside
a live object; `Arena::alloc` then double-served live memory (overlap → UAF).
Seven other linear walkers still used the wedge-prone exact-match free-block
skip; two of them WRITE while desynced (the selective-promotion evacuation walk
installs forwarding pointers which the main sweep then trusts via
`is_forwarded()` and zeroes+frees mid-live-object; `clear_all_mark_bits` writes
`gc_flags`). The fix hardens EVERY walker: shared robust skip, zero-spans are
never parsed/freed (skip to the next free-block anchor), anchor-based resync
replaces the byte-plausibility probe (which accepted all-zero headers), the
main sweep defers zeroing/publication and unwinds reclaim decisions collected
since the last anchor on any anomaly, promotion defers forwarding installs and
unwinds candidates from suspect stretches, marking/fixup walkers get
conservative base-validated fallbacks over unparseable stretches, the overlap
coalescer runs unconditionally, and forwarding targets are validated against
old gen.

Validation (2026-07-02, this dev base): baseline 1/12 CRASH (0xC0000005 at
stop-churn, 73.9s) vs fix 0/12 at `-Xmx500m`/150 s; in two fix runs the new
anomaly containment demonstrably caught a REAL corrupt header (legacy Object
with `array_len=512` — the known JIT inline-alloc header-corruption family) and
re-anchored without incident. Note today's baseline crash rate (1/12) is lower
than the historically documented 2-3/6, so the A/B count is directional; the
mechanism-level evidence (A2 breadcrumbs + the adversarially-reviewed kill
chain) is the primary case. bt16/bt18 checksums exact (14985902 / 68332206) on
both binaries; bt18 wall-time delta within background-load noise (73.8s base vs
77.3s fix on a loaded box; both ~2-3× the idle-box norm — re-measure on idle).

Known accepted residuals: (a) Layer 1 (register-invisible roots → survivable
all-zero-header stale-receiver flood) is UNCHANGED — the real fix remains
precise oop maps / shadow stack; (b) the main sweep's survivor arm still writes
mark-clear/age (2 bytes at header offsets 20/21) on a not-yet-detected suspect
stretch — cannot mint free blocks or dangle refs; (c) `walk_objects` omits
objects between an unparseable span and the next anchor (heap dumps /
histograms under-count during corruption episodes); (d) unlisted zeroed spans
are RETAINED (never re-served) until a moving cycle resets from-space —
bounded leak under sustained JIT.

The original OPEN write-up follows for the investigation record.

---

Status (historical): **OPEN.** Root-caused to the JIT×GC root-precision family; no reliable fix
landed. Both attempted conservative mitigations (full-GPR safepoint spill, shadow
stack) were **empirically insufficient**. Documented here so the dead-ends are not
re-walked.

## Symptom

`jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite1023ValidWrite1023` (suite
index 38, real-JDK, JIT on) and its large-write siblings intermittently corrupt the
non-moving young generation and crash:

```
WARN cratonvm_gc::gen_heap: GC: implausible num_slots <heap-pointer> on kind=Object header (class_id=0)
WARN cratonvm_gc::gen_heap: non-moving sweep: stopping walk at offset N — implausible object size 0
...
# EXCEPTION_ACCESS_VIOLATION (SIGSEGV) at pc=<native helper RVA>
#  Faulting access: read at address 0x0000000000000028   (= HEADER_SIZE; null-base field read at off 40)
#  thread: "Thread-NNNN"   (a Tomcat worker)
```

The corrupt "header" is 16 zero bytes followed by a run of packed 8-byte young-gen
heap pointers (no `Value` tags) — i.e. an `Object[]` element region / compact-object
body that the sweep walker reaches at a mis-aligned offset after a **live young
object was reclaimed and its slot freed/reused**. The crash is the downstream
use-after-free: a native helper called from JIT reads a reference field (offset 0x28)
of a null/dead object.

Newly exposed by the DoHead root-deposit fixes `886e2d66` + `24cf0b19`, which turned
the family's prior uniform HANG into a MIX (some variants PASS, several CRASH).

## What is confirmed

- **JIT-only.** `--nojit` runs produce **zero** corruption warnings (the class then
  just HANGs on the pre-existing HTTP/2 `testDoHeadHttp2` blocked-thread hang, which
  is a separate issue). JIT-on corrupts. So the corruptor is in / triggered by
  JIT-compiled code.
- **Deterministic-ish repro at a small heap.** `-Xmx500m` makes young GC frequent
  enough that the class crashes in **~2–3 of every 6 runs** at a 150 s timeout
  (vs. rarely at the 2 g suite default). `-Xmx300m` is too small (hangs/OOMs).
- **Crash = UAF.** Null-base field read at offset 0x28 in a native helper invoked
  from JIT — a still-live young object was collected.

## Dead ends (do NOT re-try without new evidence)

Every black-box A/B below is **confounded** two ways and must be read with N≥6 and
the crash (NOSUMMARY/FAIL) as the signal, NOT the corruption count:
1. **Corruption count tracks execution progress**, not the toggle — a run that hangs
   early on the HTTP/2 test shows 0 corruption without being "fixed."
2. **The crash itself is flaky** (~30–50% at -Xmx500m/150 s), so N=3 samples routinely
   mislead (a clean 0/0/0 batch is pure luck).

- `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all` (blind-spill the full GPR file at every
  GC-capable safepoint so the conservative scan marks every register-resident oop).
  Measured **3/6 crashes** — indistinguishable from the `=0` baseline's **2/6**.
  A default-flip of this gate was built and measured; it does not help and adds
  per-safepoint spill cost. Reverted. (An early N=3 env-A/B showed a spurious
  0/0/0 — flakiness.) `=1` (callee-saved only) also crashed.
- `CRATONVM_SHADOW_STACK=1` (precise rewritable JIT roots). Reduced but did not
  eliminate corruption (one run still 32 events); and it is explicitly EXPERIMENTAL /
  "do not enable in production" (partial moving-gen scaffolding, under-counts bt18).
- `CRATONVM_NO_SELECTIVE_PROMOTE=1` — still corrupts (96 events in one run), so the
  selective-promotion **relocation** hypothesis is refuted: the corruptor is not
  selective promotion moving a JIT-referenced object.
- `CRATONVM_PRECISE_JIT_MAPS=1` — corruption count went UP (confounded; it also
  switches on the moving young collector, which relocates more and has its own
  residual). Uninformative.
- `CRATONVM_GC_STRESS=1` — unusable: dies at startup with an unrelated
  "ServiceLoader.getName() returned null" under BOTH jit and nojit (a separate
  GC-stress startup bug), before reaching this code path.

## Root cause (CONFIRMED via `CRATONVM_DBG_A2` instrumented detector)

`CRATONVM_DBG_A2` reproduces at -Xmx500m WITHOUT suppressing the bug (unlike
`CRATONVM_DBG_SWEEP_ZERO`, whose per-dead-object ring write is heavy enough to make
the heisenbug vanish — 0 corruption in 4 runs). The A2 breadcrumb + alloc records
show:

- **`mismatch=0`**: the walker's computed size never disagrees with an object's real
  allocated size → NOT a walker/sizing bug.
- Every "corruption" is the walk landing at a **mid-object offset** inside a REAL
  live legacy object (e.g. `cursor covered by alloc class_id=1748 ns=24
  REAL_size=424 mid-object offset=240`), reached after **striding regions the sweep
  ZEROED in a prior cycle** (`prior slot was FREED by the sweep`). A 424-byte zeroed
  region reads as consecutive 40-byte all-zero `Object`s; 424 isn't a multiple of 40,
  so the walk oversteps off the object grid into a live legacy object's `Value`-cell
  data (disc=4 Object tag → the `class_id=4, array_length=1` false-positive header).
  **The walk desync is a downstream SYMPTOM, benign (it re-syncs / over-retains).**
- **The FATAL cause** is upstream: the crash is preceded by a **64,145-event flood**
  of `Stale pointer detected in invokevirtual receiver (all-zero header) — falling
  back to CP class java/util/concurrent/locks/AbstractQueuedSynchronizer$ConditionNode`
  (and `$ConditionObject`). So the objects the sweep reclaims **while still live** are
  AQS `ConditionNode`/`ConditionObject` held by **parked Tomcat worker threads** (in
  JIT-compiled `ConditionObject.await()` → `LockSupport.park`). Those regions are then
  zeroed+freed, and their zeroed spans are what desync the walk.

This is **exactly** the documented AQS blocked-thread register-invisibility residual
(`reference_tomcat_dohead_aqs_blocked_jit_register_root`): the JIT `await` keeps the
`ConditionNode` oop in a **callee-saved register** across `park`, never spilled to the
frame; during executor shutdown the AQS sync queue is torn down so the node is
heap-unreachable; and `deposit_root_snapshot`'s conservative frame scan
(`scan_active_jit_frames`, vm_exec.rs ~1287 — the 24cf0b19 fix IS present here) sees
only the frame/spill region, not the register. The large-write DoHead load amplifies
the churn, so the memory's post-fix ~2-108 flood becomes ~64k here.

Why the mitigations don't work: `SAFEPOINT_REG_SPILL=all` should spill the register at
the `park` call site, but empirically the flood/crash persists (3/6) — the spill is not
reaching the register-resident `ConditionNode` there (or `park`'s invoke does not emit
the pre-safepoint spill). Selective promotion is irrelevant (reclamation, not
relocation). The **real fix is precise oop maps / shadow stack** (deferred;
`CRATONVM_SHADOW_STACK` is experimental/non-production) — OR a targeted heap root that
keeps a thread's parked-on `ConditionObject`+node chain reachable across `park` (cf.
the BUG-W `pin_native_root` pattern for `monitor_wait`).

## How to reproduce the diagnosis

`CRATONVM_DBG_A2=1` at -Xmx500m (does NOT suppress). Then:
`grep 'ConditionNode\|ConditionObject' <FQN>.log.err | wc -l` → the flood;
`grep '\[A2\] BREADCRUMB' <FQN>.log.err` → mid-object offsets + `mismatch=0`.

Related: `reference_tomcat_dohead_aqs_blocked_jit_register_root`,
`fork6-fjp-multithread-jit-root-reclamation.md`,
`gc-blocked-thread-frame-stale-thread-mirror.md`. Precise oop maps / shadow stack are
the deferred "real fix".

## Repro

```
cd apps/tomcat-suite-runner
# crashes ~2-3 of 6 runs; use -Xmx500m and count NOSUMMARY/FAIL, not corruption warnings
.\run-tomcat-suite.ps1 -Vm craton -Jit on -Jdk real -Category all -RunName dohead `
  -Start 38 -Count 1 -TimeoutSec 150 -Parallel 1 -MaxHeap 500m -Exe <cratonvm.exe>
# logs: apps/tomcat/.suite/results/dohead/real-jit/<FQN>.log(.err)
```
`--nojit` is the clean control (never corrupts). Do NOT run bt benchmarks or other
VMs concurrently with the repro — CPU contention perturbs the timing-sensitive
window and produces exit-code-5 / false-clean runs.
