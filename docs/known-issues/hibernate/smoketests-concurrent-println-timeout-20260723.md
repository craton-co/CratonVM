# Hibernate `SmokeTests#testQueryConcurrency` — 120 s JUnit timeout

**Status:** OPEN (2026-07-23; re-diagnosed 2026-07-28). Not closable by
incremental tuning — see "What closure actually requires".

`org.hibernate.orm.test.sql.exec.SmokeTests#testQueryConcurrency` runs 50 forks x
400 iterations (20 000 transactions, 3 HQL queries each) on a 5-thread pool. It
hits Hibernate's 120-second per-method limit while the other 16 tests in the
class pass.

## The number the doc never stated

Run alone with the cap lifted (`-Djunit.jupiter.execution.timeout.default=3600s`,
`MethodRunner`), the test **passes**:

| | CratonVM | HotSpot JDK 25 | ratio |
|---|---:|---:|---:|
| `testQueryConcurrency` alone | **614 608 ms** | **15 943 ms** | **38.6x** |

So this is not a hang, a deadlock, or a correctness bug — it is throughput, and
**closing it needs ~5.2x**, not a few percent. That single fact retires the
entire history of ±10–30% experiments recorded in the older revisions of this
doc: none of them could ever have closed it.

## Reproduce it in 90 seconds, not 10 minutes

`apps/hib-suite-runner/SmokeConcProbe.java` runs the identical unit of work (same
three HQL queries, same two annotated entities, same in-memory H2) with
fork/iteration/thread counts on the command line, after a warm-up, and reports
steady-state throughput plus the extrapolated cost of the real test body:

```bash
cd C:/craton/CratonVM/apps/hib-suite-runner
# forks iterations threads warmupForks [mode]
<cratonvm> --java-home "<jdk25>" --Xmx 1500m @common.args SmokeConcProbe 6 400 5 2
"<jdk25>/bin/java.exe" -Xmx1500m @common.args SmokeConcProbe 6 400 5 2
```

Baseline: `PROJECTED_TEST_MS` ~502 000 (CratonVM) vs ~11 900 (HotSpot) — it
reproduces the 40x faithfully in ~90 s per data point. Absolute numbers on this
shared box drift up to ~50% with peer load, so always A/B back-to-back.

## Measured decomposition (this is where the older framing was wrong)

The doc used to call this "an interpreter-throughput problem in the concurrent
H2/Hibernate query path" and chased AQS/RRWL CAS cost, logging I/O, and
monitor-table caching. Measured:

| lever | effect |
|---|---|
| quiet the JDBC bind/extract TRACE logging | 502 s -> 452 s (**~10%**) |
| `--nojit` (compile nothing) | 502 s -> 524 s (**JIT is worth 4%**) |
| `CRATONVM_JIT_ALLOW_PACKAGES=org/hibernate/` | 502 s -> **1104 s (2.2x WORSE)** |
| `CRATONVM_JIT_VIRTUAL_TIERUP=0` | **neutral** (43 vs 41 txn/s) |

`CRATONVM_JIT_VIRTUAL_TIERUP=0` was previously the best known lever (+31%); that
machinery has since been fixed and the switch no longer does anything. Any
reference to it as a live lever is stale.

Per-subsystem, same 1600 transactions, `SmokeConcProbe <forks> 400 5 2 <mode>`:

| mode | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `txn` — empty transaction | 28 ms | 4335 ms | **155x** |
| `session` — session open/close, no JDBC | 9 ms | 1022 ms | **114x** |
| `parse` — 3x `createQuery`, no execution | 77 ms | 7457 ms | 97x |
| `jdbc` — raw H2 connect/begin/commit/close | 106 ms | 8516 ms | 80x |
| `exec` — one query executed | 407 ms | 13 518 ms | 33x |
| `full` — the real test body | 777 ms | 33 973 ms | 44x |

Note the shape: the *cheapest* units of work have the *worst* ratios. There is no
single hot spot to remove — the gap is uniform-to-worse across every layer,
which is the signature of general interpreted execution, not of one pathology.
(HotSpot's C2 also elides more of the trivial cases, which inflates the small-unit
ratios; the point is the absence of an outlier, not the exact multiplier.)

## What closure actually requires

A ~40x gap for interpreted bytecode against C2-compiled code is unremarkable.
The anomaly is the third row of the lever table: **compiling Hibernate makes it
1.5–2.2x slower**, and that result is steady state, not compile-time cost — it
survives an 8-fork (3200-transaction) warm-up (18 txn/s compiled vs 27 txn/s
interpreted).

So the JIT is currently a net negative on dispatch-heavy ORM code. Until that is
fixed, the only way to run this workload is interpreted, and interpreted cannot
reach 120 s. Closure is therefore gated on the tiered-manager work
(`project_wire_tiered_manager`), not on anything local to this test. Do not
spend more effort on per-call micro-optimisations here; the measurements above
bound what they can possibly buy.

### The workload is essentially never compiled

`CRATONVM_DBG=jit-method-stats` on `SmokeConcProbe 2 200 5 1 full` (note the
spelling — a bare `CRATONVM_DBG_JIT_METHOD_STATS=1` is rejected with a warning
and does nothing):

```
1642 distinct methods tracked, 1638 ever invoked, 4277121 total invocations
still-interpreted=1535  c1=32  full-profile=0  c2=75
compiles: c1=107 c2=77 osr=4 deopts=0 c2_bailouts=0 total_compile_time_ms=1
c1_threshold=500  hot_but_stuck_in_interpreter=1531
```

**1531 of 1642 hot methods never compile**, and the entire process spends
**1 ms** compiling. Every one of the top 30 stuck methods carried
`tier_fail_count=3` — the `MAX_TIER_FAIL_RETRIES` permanent ban
(`jit/src/tiered.rs`). They are trivial accessors called tens of thousands of
times: `SessionLocal.isClosed()Z` (55 348), `JdbcConnection.checkClosed()V`
(44 468), `SessionImpl.isClosed()Z`, `AbstractQueuedSynchronizer.getState()I`.
Same shape as the `action.queue` doc's root cause 2 and tomcat doc 30.

**That reading was misleading, and the admission accounting has since been
fixed** (commit `7f894b4da`). `tier_fail_count` was charged both for a compile
that ran and failed *and* for a method the VM declined on policy, so a
permanently-ineligible method was enqueued and declined three times before the
counter saturated — and the resulting `tier_fail_count=3` was indistinguishable
from genuinely broken codegen. The two are now separate, and the same line reads:

```
hot_but_stuck_in_interpreter=1519 (of which ineligible-by-policy=1513, compile-failures=6)
```

So **99.6% of the "never compiles" population is banned by design**, not by a
compiler bug: `org/hibernate/` wholesale (HIB-TEMPORAL.1), `org/h2/`
(HIB-LONGTAIL.1, `vm/src/jit/skip_list.rs`), and the
`AbstractQueuedSynchronizer` `getState`/`setState`/`compareAndSetState` family.
Those bans stand on their own correctness grounds; they are the reason this
workload is interpreted, and they are what would have to change.

The genuinely broken minority is **six methods**, now listed under their own
`COMPILE FAILED (not policy — these are bugs)` heading in the same dump
(they never appeared in the top 30, which is ranked by invocation count):

| method | tier_fail_count |
|---|---|
| `java/time/Instant.create(JI)` | 3 |
| `org/antlr/v4/runtime/atn/ATNSimulator.getCachedContext` | 3 |
| `org/antlr/v4/runtime/atn/ATNDeserializer.stateFactory(II)` | 3 |
| `java/util/IdentityHashMap.clone()` | 3 |
| `java/util/concurrent/LinkedBlockingQueue.take()` | 2 |
| `java/util/concurrent/LinkedBlockingQueue.offer(Object)` | 2 |

These are the actionable JIT bugs this workload exposes. Fixing all six would
not close this class — they are a rounding error against the 5.2x requirement —
but they are real and they are now visible.

The enqueue-churn mechanism, via `CRATONVM_DBG=jitc`: the tier manager kept
re-enqueueing methods the skip list rejects, and `background_compile_task`
declined each *before* its trace point, so **78 of 92 enqueued methods never
produced a `bg-compile` line** — enqueued at `invoc_count=500`, again at 564,
again at 628, then banned. Post-fix a decline is recorded on the first attempt.

Checked and rejected as the cause: a global `any_class_redefined()` latch
disabling all background compilation. `bg-compile` lines continue to the end of
the run, so compilation is not shut off process-wide.

Two concrete sub-defects are worth fixing on the way, both independent of the
main gap:

1. **The JIT code buffer overflows compiling large Hibernate methods.** With
   `CRATONVM_JIT_ALLOW_PACKAGES=org/hibernate/`, a short run emits ~380
   `JIT try_patch_byte/try_patch_i32: offset out of bounds; marking buffer
   overflowed` warnings across a handful of distinct buffers (`len=6238`,
   `len=15519`). Those methods silently never compile. It is not a compile
   storm and not the cause of the 2.2x regression, but it means the biggest
   Hibernate methods are permanently un-compilable.
2. **The JIT's array-allocation path does not scale across threads** (see
   below). The interpreter's does now; the JIT's still shows flat aggregate
   throughput.

## Fixed on this investigation (does not close this doc)

`AllocScaleProbe.java` showed **array allocation was fully serialised**:
aggregate `new long[16]` throughput was flat from 1 to 4 threads while
`new Object()` scaled, because `gc_alloc_array` had no TLAB path at all and
`try_alloc_young_initialized` holds the global `young_from` mutex across the
bump, the zeroing, *and* the header init — so hold time scales with allocation
size. Fixed in commit `7888b80b6` (arrays now share the object path's hardened
refill machinery; `is_humongous` no longer locks). Interpreter array allocation
gained **+36% aggregate at 4 threads**; bt18 canary showed no collapse and an
unchanged checksum (68332206); Hibernate 20-class family 117/117.

Measurement warning for bt18 specifically: its absolute time on this shared box
swings roughly **2100–4200 ms** with peer build load. A later interleaved A/B
(4 rounds, control built from the same tree with only the changed files
reverted) put two binaries at 3989 ms vs 3965 ms while single samples taken
hours apart had read 2264 ms and 3790 ms — i.e. an apparent "63% regression"
that was entirely host state. Never compare bt18 numbers across sessions; only
interleaved A/B on one host state means anything.

Effect on this test: **~3%** (382.9 s -> 372.6 s). Recorded here so nobody
re-derives it: allocation scaling was real and worth fixing, but it is not what
dominates this workload.

## Probes left behind

All in `apps/hib-suite-runner/`, all usable on any VM build:

- `SmokeConcProbe.java` — this workload, tunable, with `txn`/`session`/`jdbc`/
  `parse`/`exec`/`full` decomposition modes.
- `AllocScaleProbe.java` — allocation throughput vs thread count, `Object` and
  `long[16]`, reports aggregate scaling.
- `PrimCostProbe.java` — per-op cost of the JDK primitives a session
  open/close leans on, 1-thread vs N-thread, so a merely-slow primitive can be
  told apart from one that fails to scale.

Caveat for whoever picks this up: `PrimCostProbe`'s `ConcurrentHashMap.get` row
is confounded by `Integer` autoboxing and a native->Java `hashCode` re-entry —
do not read it as a CHM cost. The allocation rows are clean.

## Ruled out (do not re-investigate)

- Stop-the-world GC pauses — `CRATONVM_DBG_GCPAUSE` shows no pause >= 100 ms.
- Lock convoying as the primary cost — the process sustains ~4.2–4.5 of 5
  worker threads busy throughout the timing window. (Spinning on a contended
  mutex also presents as CPU-busy, so this rules out *blocking*, not all
  contention; the allocator serialisation found below was exactly such a case.)
- Logging as the driver — ~10%, quantified above.
- The Hibernate JIT ban as the cause — lifting it makes things worse, and the
  ban's own justification (HIB-TEMPORAL.1, a `DdlTypeImpl.getRawTypeName` JIT
  corruption from 2026-07-08) is a correctness guard, not a throughput one.
- `CRATONVM_JIT_VIRTUAL_TIERUP` — no longer a lever.

## Prior investigation

The pre-2026-07-28 revision of this doc recorded a long series of rejected
experiments (RRWL/AQS CAS stripe caching, monitor-table handle caches,
one-write logging, `ParserBase.testToken` natives, framework-log mirror
removal, `CRATONVM_ROOTSNAP_CACHE=0`). All landed within ±30% and none closed
the class; the 5.2x requirement above explains why, and they are not repeated
here. See git history for the detail if a specific one needs revisiting.
