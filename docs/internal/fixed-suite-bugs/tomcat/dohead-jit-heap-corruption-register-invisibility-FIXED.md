# Tomcat `TestHttpServletDoHead*` — JIT-only young-gen heap corruption / SIGSEGV

**Status (2026-07-13): RESTORED to known-issues.** This doc was previously
retired to `../..` under a `-FIXED` filename because its two
documented root causes were genuinely fixed and merged to `dev`:

- **Root cause #1** (young from-space walk-desync / GC corruption, "FATAL
  LAYER" below) — commit `928cc5b3`, confirmed present in current `dev`
  history (`git merge-base --is-ancestor 928cc5b3 origin/dev` succeeds).
- **Root cause #2** (`StreamEncoder` eager-flush breaking the commit
  threshold) — commit `1773d3df2` ("fix: buffer StreamEncoder writes to
  match real HotSpot flush granularity"), also confirmed present in `dev`.
  See `dohead-streamencoder-eager-flush-commit-threshold.md` (also restored
  from `..`).

**2026-07-13 cross-reference note (CORRECTED):** the `String.setOption`
signature below was hypothesized (in `largeclienthello-string-size-nosuchmethod.md`,
now fixed and moved to `docs/internal/tomcat-08-07/`) to share a root cause
with an unrelated `NoSuchMethodError: java/lang/String.size()I` in
`ClassLoaderLogManager.resetLoggers()`. That specific hypothesis is
**refuted** — the `resetLoggers` bug was a deterministic, unrelated
real-vs-synthetic `java.util.logging.Logger` field-slot collision (fixed;
see that doc's "Refuted hypothesis" section).

However, the `Socket.setSoTimeout` family here is **not** the deep
register-invisible-root/GC family either — a concurrent 2026-07-13
investigation ([[project_dohead_third_cause_socketfactory_synthetic_20260713]]
in project memory) root-caused it **deterministically**: commit `be6055605`
(2026-07-09) added synthetic `javax/net/SocketFactory.createSocket`
natives that fabricate a 5-slot synthetic-layout `java/net/Socket`; under
`CRATONVM_REAL_NET_SOCKETS=1` (set by the Tomcat/Spring suite runners) every
`java/net/Socket` native is dropped so *real* `Socket` bytecode consumes
that synthetic object — a producer/consumer layout split-brain. Which of
the three faces (`String.setOption`, `socketLock`-null NPE, `Socket is
closed`) you hit depends on which garbage byte pattern lands in which real
field slot for a given call path (e.g. `useAsyncIO` true/false take
different construction routes) — reproducible and JIT-independent
(reproduces with `--nojit`), not a GC-timing race. A minimal
`new Socket(host,port)` probe doesn't reproduce it because that goes
through `Socket`'s own real constructor directly, never through the buggy
`SocketFactory.getDefault().createSocket(...)` producer path that
`Http2TestBase.openClientConnection` actually uses.

**Update 2026-07-13 (evening): the whole "third root cause" is FIXED**
(branch `fix/dohead-family-regressions-v2-20260713`). It was THREE
separate regressions from the 2026-07-09 WildFly bootstrap batch, none of
them a GC/JIT bug:

1. **The Socket cluster above** — fixed by extending the RNS registry
   drop-filter to `javax/net/SocketFactory` (exactly the fix the
   concurrent investigation prepared), so real factory bytecode
   constructs sockets through real `Socket` constructors.
   `javax/net/ServerSocketFactory` deliberately kept.
2. **8 `testDoHead` `expected:<2> but was:<3>` failures** (+ their Http2
   pairs; params 46/47/58/59/118/119/130/131 — exactly the original
   commit-threshold family of
   `dohead-streamencoder-eager-flush-commit-threshold.md`): `b448f2039`
   added an ungated `java.io.OutputStreamWriter` native surface that
   shadowed the real OSW bytecode (WP0.1) and bypassed the StreamEncoder
   shim's batching entirely — the eager-flush bug reintroduced one layer
   up. Fixed by gating that surface to synthetic-JDK builds (see the
   updated eager-flush doc for the standalone 1024×16-vs-32×512 repro).
3. **The `String.size()I` flood** from juli `resetLoggers` — the
   `java.util.logging.Logger` handler-natives slot-2 collision, fixed
   independently on dev (`d94712f2a`, see
   `docs/internal/tomcat-08-07/largeclienthello-string-size-nosuchmethod-FIXED.md`).

Post-fix validation (Windows suite runner, the same environment as the
07-12 rerun): `TestHttpServletDoHeadInvalidWrite1024ValidWrite512`
288 run / 286 pass and `InvalidWrite1023ValidWrite1023` 288 run / 287
pass, with ZERO occurrences of any of the three signatures; the remaining
failures are load-flake shaped (WinSock 10053 connection abort mid-read,
HEAD read-timeout, Tomcat lifecycle start/stop under churn) at
non-deterministic parameter indices — the environmental family of the
retired `dohead1023-http2-index0-socketexception-likely-host-contention.md`
analysis (that doc's host-contention theory was right for ITS 2/288
flakes; the deterministic 152/288 cluster was the regressions above).

**Full-family validation (2026-07-13, later the same day):** all **64**
`TestHttpServletDoHeadInvalidWrite*` classes (suite indices 28–91, 18,432
tests total) were swept on a current-`dev` binary (includes the fixes
above plus the STW-takeover bracketing fix `945e44920` and the
SyntheticStub dispatch-gate hardening `c73eeda7b`), real JDK, JIT on,
1200s timeout, parallel 2:

- **58/64 PASS clean (288/288); 6 classes at 287/288**, each with exactly
  one failure at a random parameter index, in two shapes: 3× HTTP/2
  mid-read disconnect (`IOException: End of input stream with [9] bytes
  left`, the retired host-contention doc's environmental family) and
  3× `LifecycleException: Protocol handler start failed` in test setUp —
  root-caused during the solo re-runs to a sporadic
  `IllegalThreadStateException` from `Thread.start()` on a freshly
  constructed Tomcat endpoint worker (~1/5000 Tomcat boots, a CratonVM
  Thread-state-tracking bug, NOT DoHead-specific and not port churn) —
  filed as `docs/known-issues/tomcat-08-07/
  threadpoolexecutor-prestart-illegalthreadstate-sporadic.md`. Solo
  re-runs of the six classes otherwise PASS
  (`apps/tomcat/.suite/results/dh3rerun/`).
- **Zero occurrences across all 64 err logs** of: the AQS
  ConditionNode/ConditionObject stale-receiver flood (Layer 1 — the
  parkBlocker pin holds), any `Stale pointer` fallback, any walk-desync /
  zero-span / overshoot / bad-forward containment event (Layer 2 — the
  hardened sweep never even engaged its anomaly paths), any crash marker,
  any `String.size()I` NSME (Logger fix holds), and any STW-takeover-wait
  warning (the `945e44920` bracketing fix holds).
- The only GC-adjacent events were 154 `mark_young: rejecting object with
  implausible extent` lines (~2.4 per 288-test class) — the extent-clamp
  (`8e64d9a5`) conservatively rejecting non-object conservative-scan
  candidates, its designed retention-safe behavior, with no downstream
  anomaly in any run.
- Historical note on the earlier flake floor: an identical sweep hours
  earlier on a pre-`945e44920` binary showed ~70% of classes with 1–2
  `SocketTimeoutException: Read timed out` (300 s client timeout!)
  failures plus one 1200 s HANG stuck at `STW cross-thread JIT takeover
  is still waiting for cooperative mutators` — and the 07-12 baseline run
  had 33 such timeouts across DoHead AND unrelated classes
  (TestELInJsp, TestRewriteValve, TestHttp11Processor…). The takeover
  bracketing fix eliminated all of them (sweep wall time dropped from a
  projected 5+ h to 71.8 min on the same loaded box), identifying the
  STW-takeover wedge — not host contention — as the dominant source of
  that long-standing cross-suite read-timeout flake family.

The original 2026-07-12 finding follows for the record: a fresh
full-suite rerun on `dev` (commit `080e79256`, 2026-07-12, real JDK, JIT
on, 1200s timeout) showed all ~19
`jakarta.servlet.http.TestHttpServletDoHeadInvalidWrite*` classes FAIL or
HANG — none PASS — with
`TestHttpServletDoHeadInvalidWrite1024ValidWrite512` at **152 of 288
failures**:

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
