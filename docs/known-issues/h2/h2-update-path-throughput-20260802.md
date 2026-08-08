# The H2 UPDATE path costs ~30-60x HotSpot's CPU per update, and ~87x over the whole class

## Status
**OPEN (2026-08-02).** Successor to the retired
`bug-h2-testmultithread-concurrent-update-timeout` write-up, which is retired
because everything on it that was a *defect* is closed: the class died in three
seconds on dev tip (two IR-tier JIT bugs, fixed), H2's own `LOCK_TIMEOUT` /
`job.get` timeouts were the slowness surfacing rather than a bug, and the
`CloneNotSupportedException` residual moved to
`bug-h2-classid0-stale-address-family.md`.

What is left is this page: a constant factor, re-measured on shapes that can
carry it, and a thread-scaling question this host cannot answer. It is
deliberately **not** filed as "a bug" — the previous framing ("≈100x. That is
the bug.") pointed three sessions at a problem no single fix could match.

## UNBLOCKED 2026-08-07 (was: no file-backed H2 database opened on dev tip)

Two defects from the `HEADER_SIZE 24 -> 16` landing blocked this page for most
of a day. Both are fixed on dev and **re-verified here on the merged tree**
(`cc86228dd`), so the measurements below can be re-taken in the default
configuration:

| defect | fix | witness, re-run on the merged tree |
| --- | --- | --- |
| `FileChannel.tryLock()` read a corrupt `fileLockTable` cell, so no file-backed database opened | `monitorexit` erased the object header quartet, converting a compact object to "legacy" on the first `synchronized` exit — one line in `vm/src/threading/monitor.rs` | `probes/CompactLayoutFileLockProbe.java` -> `PROBE-OK`; `MergeLockBudgetProbe verify 50` on a real H2 file DB -> `PROBE-OK` |
| a `WeakReference` whose referent died was cleared but never enqueued, so nothing came out of a `ReferenceQueue` | "young and unmapped" is not a death certificate: after a NON-MOVING sweep nothing moves, so the map is empty and every live young `Reference`/`ReferenceQueue` was condemned — `pre_gc_addr_did_not_survive` now asks `is_live_young_survivor` | `probes/EnqProbe.java` -> `gc enqueued it = true`, 3 of 3 and with `--nojit` |

The second one matters directly to this page: **the non-moving sweep is the only
young-collection path these workloads take** (`reason=unregistered-jit-frame-on-
stack`, `compiled-frame-oop-not-published`, `innermost-rbp-belongs-to-unguarded-
callee` — the same fallback §"Where the CPU goes" already names as a scaling
target), so GC-driven enqueue never happened at all in any H2 measurement taken
before this fix.

**The 2026-08-07 profile below still carries its `CRATONVM_COMPACT_REF_FIELDS=0`
caveat** — it was taken while the first defect was open, so it is valid for the
dispatch / GC-root / class-resolution questions it is used for and must be
re-taken with packing on before being compared against anything measured with
packing on.

### The five-class gap is not closed by either fix

The enqueue fix was recorded as the sibling defect that would recover the five
H2 classes the quartet fix left short of the pre-landing baseline. Measured on
the merged tree, `--Xmx 1g`, it does not:

| class | before the enqueue fix | merged tree |
| --- | --- | --- |
| `TestLob` | FAIL, `OutOfMemoryError: Java heap space` | **HANG** at the 400 s cap |
| `TestMemoryUsage` | PASS | PASS |
| `TestLIRSMemoryConsumption` | PASS | PASS |

`TestLob` moved from OOM to timeout — consistent with the enqueue fix relieving
real memory pressure — but it still does not pass, and the other two were
already green. Whatever the remaining five are, they are not this.

### A caution for anyone A/B-ing this suite

**What did NOT change: the constant factor.** ABBA-interleaved, `--Xmx 4g`, on a
quiet host (load 6.7-8.8), work term for 10 000 updates after subtracting an
interleaved 0-update baseline of the same shape:

| build | baseline | with updates | work term |
|---|---|---|---|
| pre-landing `1082eb446` | 15.44 | 37.41 | **21.97 CPU-s** |
| current dev | 16.19 | 38.64 | **22.45 CPU-s** |

+2%, inside noise. Against HotSpot's ~1.13 CPU-s on the same shape that is
**~20x at 4 threads**, where the table below says ~31x — the difference is the
host, not the VM: this page's setup figure of 36-54 CPU-s is 15-16 CPU-s on a
quiet one. Quote the ratio, never the absolute, and record `uptime`.

**The thread-scaling experiment was blocked on the OOM, and is not any more.**
When this was written the arm size this page specifies (~100 000 updates)
exhausted the heap before finishing. Fixed 2026-08-07 in `0ea21c07a` -- 4
threads x 60 000 updates at `--Xmx 4g` now completes in 163 s. The experiment
is runnable; see the scaling section below.

**The probe is not missing, and it has moved.** `H2UpdateScaleProbe.java` was in
`docs/internal/repros/h2-insert-scale-20260731/` — the Reproducing block below
names no path, and `apps/` is gitignored, so it was invisible from the obvious
place. It now lives at **`probes/H2UpdateScaleProbe.java`**, out of a directory
slated for deletion. Its third argument is `objectCount`, not the lock timeout.

A 40-class A/B of the enqueue fix, run 4-way parallel at a 180 s cap, showed
30 PASS on both arms and three apparent changes: `TestAnalyzeTableTx`
PASS->FAIL, `TestBigResult` HANG->PASS, `TestLargeBlob` HANG->FAIL. Re-run **in
isolation**, ABBA-interleaved, 6 runs per arm, every one of the three is flaky on
*both* arms with overlapping distributions (PASS/FAIL/HANG: 4/2/0 vs 4/1/1,
3/0/3 vs 4/0/2, 0/5/1 vs 0/4/2). None is a regression and none is a recovery.
Read on its own, the parallel run would have reported one regression and one fix,
and it is neither — which is the standing rule in
[`h2database-suite-runner`](../../../apps/h2database-suite-runner/run-h2-suite.md)
and is worth restating because it cost a full re-run to establish.

## Settled 2026-08-08: the scaling slope, the profile, and load_class_concurrent

The OOM that blocked all three is fixed (`0ea21c07a` + `95fee50a4`). With it
gone, the three questions this page left open are answerable, and two of them
are now answered against measurement rather than inference.

### The thread-scaling slope — this page's "what WOULD settle it"

Work term per update, with an interleaved 0-update baseline of the same shape
subtracted, swept twice in OPPOSITE order so host-load drift cannot fake a
slope. 16-core host; at 8 threads it was NOT oversubscribed (background load
3-5).

| threads | sweep A (load 7-10) | sweep B (load 3-5) |
|---:|---:|---:|
| 1 | 3.20 CPU-ms/update | 1.95 |
| 2 | 2.26 | 2.02 |
| 4 | 2.56 | 2.29 |
| **8** | **8.80** | **7.99** |

**Flat to 4 threads, then a ~3.5x cliff at 8**, reproduced at two different
host loads. Wall throughput does not merely stop scaling, it INVERTS: 1176
updates/s at 4 threads, 261/s at 8.

The cliff is contention, not scheduling: user CPU dominates (505 s user vs 19 s
sys at 8 threads) while **voluntary context switches rise ~40x** (68-74k at 4
threads, 1.9-2.7M at 8). That is spin-then-park on a contended lock. Naming the
lock is the next question; it is not the same question as this page's constant
factor.

### Refreshed profile — flat, and MVStore's

`perf` is unavailable on this host (`perf_event_paranoid=4`), so the page's
original method cannot be repeated. The in-VM sampler is the supported
substitute and its own flag documentation says to pair it with `--nojit`, since
JIT frames never reach the dispatch loop. `--nojit --stack-sample-ms 20`,
4 threads x 3000 updates, 7314 samples, aggregated on the DEEPEST frame per
sample (`depth=0` is the outermost frame -- aggregating that reports only
`ThreadPoolExecutor$Worker.run` and says nothing):

| samples | leaf |
|---:|---|
| 227 | `org/h2/mvstore/RootReference.<init>` |
| 193 | `org/h2/mvstore/WriteBuffer.ensureCapacity` |
| 185 | `org/h2/mvstore/Page.getKeyCount` |
| 173 | `org/h2/mvstore/Page$Leaf.getValue` |
| 136 | `org/h2/mvstore/tx/CommitDecisionMaker.decide` |
| 128 | `org/h2/mvstore/Page$NonLeaf.setChild` |
| 124 | `org/h2/mvstore/tx/Transaction.markStatementEnd` |

**There is no hotspot.** The top leaf is ~3% of samples and the profile is a
flat spread across MVStore B-tree and per-commit transaction machinery --
exactly the shape "one commit per UPDATE" predicts. Nothing here argues for a
single fix; the constant factor is spread across the whole path.

### load_class_concurrent at 1.4% "in steady state" — not reproduced

Two independent lines, both negative:

* The profile above contains **no class-loading frames at all** in 7314
  samples.
* A call counter on `SharedVm::load_class_concurrent_for`, reporting every
  4096 calls, fired **zero times** across a 71 s / 80 000-update run.

Caveat, stated because it changes what the second line proves: fewer than 4096
calls while loading the whole of H2 is implausible, so the more likely reading
is that this entry point is NOT the one the interpreter and JIT resolve
through -- `ClassManager::load_class` is the candidate. Either way the original
attribution needs re-deriving before anyone spends effort here; it is not
established that this function costs 1.4% of anything in steady state.

Worth recording for whoever picks it up: `runtime::diagnostics::classes_loaded`
is declared, reset, formatted and unit-tested, and is **never incremented by
the class loader**. A counter that is never incremented reads as a confident
zero, which is why this item survived unmeasured for so long.

## Severity
**MEDIUM.** No incorrect behaviour, but not benign either. The class takes
20-45 minutes of CPU where HotSpot takes 17 seconds, and H2's internal
`LOCK_TIMEOUT` is a **wall-clock** 10 s — so on a busy host the gap turns into
an outright test failure rather than just a slow one. One of three verification
runs came back `rc=1` with `Timeout trying to lock table "TEST"` at
`TestMultiThread.java:414`, on a host at load 57-156 whose system time exceeded
its user time. The class is neither deterministically broken nor
deterministically green, and it will stay that way until the gap closes.

**And 10 s is the roomy case.** `TestAll.lockTimeout` defaults to **50 ms**
(`TestAll.java:358`) and `TestDb.getURL` appends it to every test URL, so most
of the suite runs on a budget 200x tighter than `TestMultiThread`'s. At 50 ms
the gap stops flapping and simply fails: `TestTransaction` is **10 of 10 FAIL**
and **10 of 10 PASS** at `lockTimeout=500`, nothing else in the class broken.
That is the subject of the retired
`bug-h2-testtransaction-merge-using-lock-timeout-RESOLVED-20260807` write-up,
which had been filed as a `MERGE ... USING` correctness bug before it was
measured.

## The numbers

`H2UpdateScaleProbe` models `testConcurrentUpdate` exactly: same `NUMBER(18,0)`
PK schema, same 10 000-row `MERGE` seed, same
`UPDATE account SET balance=? WHERE id=?` + `commit` inner loop, same
`LOCK_TIMEOUT=10000`. **Both shapes below do 10 000 updates**, so the work term
dominates the baseline; 0-update runs of the identical shape are interleaved as
ordinary arms and subtracted. Median of 3, load 15-35 recorded per run.

| shape (10 000 updates) | HotSpot jdk-25 | cratonvm | ratio |
| --- | --- | --- | --- |
| 4 threads × 2500 | 0.17 - 0.22, median **0.21** | 3.8 - 9.3, median **6.6** | ~**31x** |
| 25 threads × 400 | 0.15 - 0.19, median **0.17** | 7.5 - 10.6, median **9.5** | ~**58x** |

(cratonvm figures are the pre-fix arm, n=6 and n=9 runs across four campaigns at
loads 8-43; the post-fix arm is ~10 % lower, see the retired page's A/B.)

The whole-class number is the trustworthy one, because it is a single comparison
of two runs on one host in one hour rather than a difference of two large
quantities: **1303-1506 CPU-s vs 17.3, ≈75-87x** over three cratonvm runs.

### The thread-scaling slope is NOT resolved, and the old page's was not either

The retired page claimed cratonvm's CPU per update *doubles* from 4 to 25
threads (3.6 → 7.8) while HotSpot's falls (0.41 → 0.15), and named that as the
UPDATE path's distinguishing feature against the INSERT half's flat scaling.

**That does not reproduce.** Across four campaigns the per-rep 25t ÷ 4t ratio
came out 0.84, 0.98, 1.35, 1.47, 1.56, 1.67, 1.78, 2.27 — it does not even hold
its direction. The medians give 1.44x, but the two bands overlap outright
(4 threads 3.8-9.3, 25 threads 7.5-10.6), so the median is not evidence.

The reason is structural rather than bad luck, and it is worth writing down
because it applies to every cross-thread-count comparison on this host: **at
equal total work the two shapes have very different wall durations** — ~10
minutes at 4 threads against ~1 minute at 25 — so running them back-to-back
inside a rep does not make them see the same load. Pairing removes a level
shift; it cannot remove two arms sampling different load windows.

What WOULD settle it, and has not been done: a 4-thread arm doing ~100 000
updates (~11 CPU-minutes of work against a ~40 CPU-s baseline, a 16:1 ratio
instead of 1.5:1), interleaved with a 25-thread arm of the same total work, on a
host under 10 % load or on a dedicated one. Everything smaller has been tried.

Until then the honest statement is the first table: **~30x at 4 threads and
~60x at 25**, with the growth between them real-looking but unproven.

## Interpreter against interpreter, the factor is ~10x — and it is flat

The headline 30-87x above is measured against HotSpot **with C2**, which folds
two different things together: what CratonVM's interpreter costs, and what
CratonVM's JIT fails to recover. Splitting them (2026-08-07, from the
`testMergeUsing` investigation) says where to aim and where not to.

`apps/h2database-suite-runner/probes/MergeLockBudgetProbe.java bench 500 3`,
single-threaded so wall clock is defensible, ABBA-interleaved arms (A B B A A B),
median of 3, load 5-18:

| 500 ops on a 500-row table | HotSpot `-Xint` | cratonvm `--nojit` | ratio |
| --- | --- | --- | --- |
| `MERGE ... USING` | 64.6 ms | 693.5 ms | **10.7x** |
| `UPDATE ... WHERE id=?` | 50.2 ms | 495.6 ms | **9.9x** |
| `SELECT ... WHERE id=?` | 32.8 ms | 325.1 ms | **9.9x** |
| `INSERT VALUES (?, ?)` | 24.6 ms | 254.2 ms | **10.3x** |

Two things follow.

**The factor is flat across statement kinds.** MERGE, UPDATE, SELECT and INSERT
all land within 10 % of each other, so no H2 statement path has its own
pathology on top of the general one — which is what ruled out a MERGE-specific
defect in the retired `…merge-using-lock-timeout…` write-up, and is worth
re-using: a per-statement ratio that stands out from this band is a real lead,
and one that sits inside it is this page.

**The interpreter is only half the gap.** The same MERGE costs 6.7 ms under
HotSpot C2, so HotSpot's own JIT is worth ~9.6x on this shape — almost exactly
the size of CratonVM's interpreter deficit. cratonvm's JIT recovers ~1.3-1.7x of
it (647 / 435 ms with the JIT against 757 / 706 ms `--nojit`, ABBA, n=2 each),
not ~10x. That is consistent with the deliberate interpreter-first threshold in
`vm/src/runtime/interpreter/dispatch_static.rs` ("rather than paying CratonVM's
currently-slower JIT'd dispatch for code that never amortizes the switch") —
H2's SQL execution is exactly the call-heavy, shallow, polymorphic code that
comment is about. **So roughly half of the 30-87x is a JIT that does not reach
this code, not an interpreter that is slow**, and the two halves want different
work.

A caution on scale, because it changes which half matters: the JIT needs 500
invocations per method to warm up (`CRATONVM_JIT_THRESHOLD`), and a real H2 test
statement runs 50-1000 times. The 500-op probe above is at the optimistic end.
`testMergeUsing`'s 50 merges never warm up at all, which is why its failure is
identical with and without the JIT.

## The INSERT loop lands in the same band, and its profile is flat (2026-08-07)

Inherited from the retired
`bug-h2-mvstore-insert-loop-perf-hang` write-up, which had filed the same
constant as a separate, larger "cliff". It is not separate and it is not
larger.

`apps/h2database-suite-runner/probes/H2InsertLoopProbe.java` models
`TestTempTables.testAnalyzeReuseObjectId` exactly — one connection, one local
temporary IDENTITY table, one `PreparedStatement`, 10 000 autocommit
`insert into test default values`, phases timed apart so the ~40 CPU-s
start-up tax is not folded in. `--Xmx 1g`, real-JDK 25, single-threaded, warm
rep, load 15-20:

| 10 000-row insert loop | time | µs/row | vs C2 | vs `-Xint` |
| --- | --- | --- | --- | --- |
| HotSpot 25, C2 | 15.4 ms | 1.5 | 1x | 0.013x |
| HotSpot 25, `-Xint` | 1 208 ms | 121 | 78x | 1x |
| cratonvm, JIT | 8 565 ms | 857 | **556x** | **7.1x** |
| cratonvm, `--nojit` | 12 649 ms | 1 265 | 821x | **10.5x** |

**10.5x interpreter against interpreter** is dead centre of this page's flat
9.9-10.7x band, so INSERT has no pathology of its own either. The 556x against
a default HotSpot is the same two-halves story this page already tells, with
the halves unusually lopsided: **C2 is worth 78x on this shape** (a tight, hot,
monomorphic loop around one prepared statement is close to its best case) where
cratonvm's JIT is worth 1.5x. That single number is why the insert page read
its ratio as a distinct cliff.

It is also a warning about which ratio to quote. The same four arms taken on
the same host at load 15-20 instead of 8-16 read 74 / 1 919 / 10 449 / 16 615
ms — the C2 column moves from 556x to 141x while the `-Xint` column barely
moves (10.5x to 8.7x). **HotSpot's C2 arm is the load-sensitive one**, because
it is the only arm short enough for scheduler noise to dominate. Compare
interpreters.

**The profile is flat, which is the answer to "find the dominant cost".**
`--stack-sample-ms 20 --nojit`, 887 samples over one 10 000-row loop,
aggregated by deepest interpreted frame: the heaviest leaf is
`org.h2.mvstore.RootReference.<init>` at **4.1%**, then
`tx.CommitDecisionMaker.decide` 3.5%, `Page.getKeyCount` 3.4%,
`Page$Leaf.getValue` 3.2%, `tx.Transaction.markStatementEnd` 2.6%, and forty
more entries none of which reaches 1.5%. Every one is H2's own bytecode. The
insert page's four named suspects come out at:
`Page.clone` **1.0%**, `MVMap.operate` **1.0%**, `TransactionMap` nowhere in
the top 25, and boxing under 1%.

**Natives are ~3%, not the wall.** `--dump-native-registry` over 20 000 rows:
5 812 476 invocations, i.e. 290 per row, which at the in-tree funnel
profilers' ~120 ns per compiled-code native call is ≈0.70 s of a ≈21 s
two-rep loop. The top entries are the MVMap CAS loop exactly where H2 puts it
— `AtomicReference.get` 544 288, `Enum.ordinal` 360 941, `AtomicLong.get`
346 162, `AtomicReference.compareAndSet` 341 641.

Read the two together, never the sampler alone: a native makes no interpreted
frame, so `--stack-sample-ms` charges its cost to the calling Java method.

## Where the CPU goes, and why no symbol on this list is the answer

`perf record -F 199 -g --call-graph=dwarf`, 25 threads × 1000 updates, 27 K
samples, `--sort symbol`:

| cluster | share | symbols |
| --- | --- | --- |
| Rust-side allocation | 5.9% | `_mi_page_malloc_zero` |
| dispatch + JIT precedence | ~12% | `invoke_on_class_shared_inner` 2.42, `execute_invokevirtual_cached` 1.83, `InvokeCache::get` 1.45, `try_jit_compile_callee` 1.38, `jit_invoke_virtual_mic` 1.18, `force_native_over_real_jdk_bytecode` 1.06, `virtual_dispatch_target_cached` 1.02, `find_method_recursive` 1.01 |
| GC conservative root scan | 6.6% | `native_stack_has_jit_frame` 2.76, `scan_one_frame` 2.01, `is_object_address` 1.87 |
| native-method registry lookup | 6.0% | `slot_for_exact` 2.12, `__memcmp_evex_movbe` 2.79, hashbrown search 1.13 |
| ClassManager lock | 5.2% | `RawRwLock::lock_shared_slow` 1.47, `OrderedPlRwLock::read` 1.41, `load_class_concurrent` 1.40 |
| interpreter | 3.0% | `execute_frame_from_index` |

That is ~39 % of the profile. **Removing all of it is under 2x, against 30-87x.**
Treating the list as a bug list is the mistake this page exists to stop
repeating.

The named `ClassManager` target from the old page **is done**: the invoke slow
path took two `read()` guards on the same `Class` per call and now takes one,
worth a median ~10 % of CPU per update (10 paired runs; range 0.74-1.25, so a
single pair on this host proves nothing).

The two entries that are genuinely *scaling* rather than constant-factor work,
and so are the next targets:

* **`load_class_concurrent` at 1.4 % in steady state**, hundreds of seconds after
  warm-up. Nothing should be resolving classes then; find out what is.
* **the conservative root scan** is per-thread-stack work per collection, so it
  grows with (threads × collections). Every young collection in this workload
  falls back to the non-moving sweep — `reason=unregistered-jit-frame-on-stack`,
  `compiled-frame-oop-not-published`, `innermost-rbp-belongs-to-unguarded-callee`
  — which is its own question and has its own pages.
  **RESOLVED 2026-08-07 (`0ea21c07a`).** The escalation below was right that
  this had stopped being a throughput tax, and wrong about which half was at
  fault: the fallback FRACTION barely moved (75% -> 99.4%), the collection
  COUNT exploded. The fallback is still pre-existing and still open; what was
  new is that the non-moving sweep abandoned the arena on an all-zero span,
  because a JIT-allocated `new Object()` now has an all-zero header. Fixed;
  `H2UpdateScaleProbe` 4t x 60000 at `--Xmx 4g` goes from OOM to PASS in 163 s
  and 5038 minor GCs to 11. The thread-scaling arm below is UNBLOCKED.
  **Superseded 2026-08-07: this is no longer a throughput tax, it is the OOM.**
  The fallback rate against this exact workload went from 3 to 361, and with it
  the run stopped finishing. It is now the top item on this page, not a
  footnote to it — see the re-measurement above.

## The same profile at 1 thread, on 2026-08-07 code

The table above is 25 threads on 2026-08-02 code. This is **1 thread** on
`51d68e1b7`, `CRATONVM_COMPACT_REF_FIELDS=0`, `H2UpdateScaleProbe 1 60000
10000` — 60 000 updates in 279 s against a 37 s setup, so steady state is ~88 %
of the samples. `sudo perf record -F 99 -g --call-graph=dwarf`, 34 K samples,
`--sort symbol --no-children`. (`perf_event_paranoid` is 4 on this host, so perf
needs `sudo -n`; do not change the sysctl, it is shared.)

Single-threaded on purpose: it removes contention from the picture, so the
difference between this list and the 25-thread one *is* the contention term.

| self | symbol | vs the 25t list |
| --- | --- | --- |
| **5.80 %** | `gen_heap::is_object_address` | 1.87 % — now the single largest symbol |
| 3.75 % | `interpreter::execute_frame_from_index` | 3.0 % |
| 3.26 % | `__memcmp_evex_movbe` | 2.79 % |
| 2.62 % | `_mi_page_malloc_zero` | 5.9 % |
| 2.29 % | `dispatch_virtual::execute_invokevirtual_cached` | 1.83 % |
| **1.60 %** | `JitCache::invalidate_for_class` | **not on it** |
| 1.51 % | `jit::helpers::try_jit_site_cached_native_dispatch` | not on it |
| 1.50 % | `jit::helpers::forward_jit_reference_args` | not on it |
| 1.49 % | `vm_exec::invoke_on_class_shared_inner` | 2.42 % |
| 1.38 % | `InvokeCache<JitMethod>::get` | 1.45 % |
| **1.26 %** | `field_layout::object_body_size` | **not on it** (new code) |
| **1.26 %** | `value::record_object_ref_payload_slow` | **not on it** |
| 1.16 % | `NativeMethodRegistry::slot_for_exact` | 2.12 % |
| 1.12 % | `resolve_field_ref_loader_aware` | not on it |
| 0.94 / 0.91 / 0.68 / 0.65 % | `validate_code_ptr`, `pin_jit_code_range_owner`, `JitCache::get`, `compute_jit_key_hash` | not on it |
| **0.64 %** | `SharedVm::load_class_concurrent_for` | **1.4 %** |

### What this changes about the two named next targets

**`load_class_concurrent` is a lock-contention term, not a class-loading one.**
It is 1.4 % at 25 threads and **0.64 % at 1**, on a workload whose steady state
resolves no new classes in either shape. A cost that halves when the threads go
away is contention on the `ClassManager` read lock in the fast path, not work
being done. The old framing — *"nothing should be resolving classes then; find
out what is"* — asks the wrong question: the answer is "almost nothing is, and
the 1.4 % is 25 threads queueing to find that out." The work item is the lock,
which is the same item the page already closed once (two `read()` guards per
invoke down to one) and evidently not all the way.

**The conservative root scan is confirmed, and bigger than it looked.**
`is_object_address` alone is 5.80 % single-threaded, above the whole
GC-root cluster's 6.6 % at 25 threads. This one does not need contention to be
expensive, and it is the clearest single target on the list.

**Three clusters on this list are not on the old one at all**, which is what a
five-day-old profile of a moving codebase is worth:

* **JIT bookkeeping, ~5.4 %** — `invalidate_for_class` 1.60 %,
  `try_jit_site_cached_native_dispatch` 1.51 %, `forward_jit_reference_args`
  1.50 %, plus `validate_code_ptr` / `pin_jit_code_range_owner` /
  `JitCache::get` / `compute_jit_key_hash` at ~3.2 % between them. On a
  single-threaded run that is already past warm-up, `invalidate_for_class` at
  1.6 % deserves its own look: something is invalidating compiled code in steady
  state.
* **Layout computation, 1.26 %** — `object_body_size` is new code from the
  header change and is being called on a hot path.
* **`record_object_ref_payload_slow`, 1.26 %** — a `_slow` suffix at over 1 %
  is usually a fast path that stopped being taken.

Same caveat as the old list, and it is the whole reason this page exists:
**that is ~30 % of the profile and removing all of it is under 1.5x, against
~10x.** These are targets, not a bug list.

### Methodology: do not trust the caller graphs on this binary

`--call-graph=dwarf` unwinds this build badly enough to be misleading, not just
incomplete. Asking for the callers of `JitCache::invalidate_for_class` returns
it *underneath* `RawVecInner::finish_grow` underneath `pin_native_root` — an
incoherent chain, produced by unwinding through deeply inlined Rust. The flat
self-attribution above needs no unwinding and is sound; every caller-side claim
from this data set was discarded. If a caller question has to be answered, it
needs an in-VM counter, not perf.

## Setup cost

10 000 `MERGE` + VM start + H2 class load, single-threaded: HotSpot **2.2-3.0
CPU-s**, cratonvm **36-54 CPU-s** on this host at load 15-30. The spread is the
host, not the VM: four runs of the identical 0-update shape, minutes apart, came
back 29 / 36 / 43 / 50 CPU-s. A flat tax on every H2 run, and the reason a
"1-second" H2 test costs half a minute here.

## What is ruled out (do not redo)

* **`MERGE ... USING` is not a defect and not a slow path.** Its row state is
  byte-exact on both branches (9 of 9 checks, with and without the JIT), and its
  ratio sits inside the flat band above. `TestTransaction`'s
  `Expected: 100 actual: 50` is this page's constant factor tripping a 50 ms
  `LOCK_TIMEOUT`; stock HotSpot produces the identical shortfall when its budget
  is scaled down to 1-3 ms. See the retired
  `bug-h2-testtransaction-merge-using-lock-timeout-RESOLVED-20260807` write-up.
* **There is no `org/h2/` JIT package ban to lift.** Measured 2026-08-02 with
  `CRATONVM_DBG_JIT_COMPILED=1`: **27** `org/h2/…` methods JIT-compile on the
  default build, **26** with `CRATONVM_JIT_ALLOW_PACKAGES=org/h2/`. The flag is a
  no-op for this workload, so the retired insert page's "lifting the ban made it
  ~9 % worse" was a **null A/B** — two identical configurations — and is
  withdrawn.
* ~~**Not heap pressure** (`--Xmx` 1g/2g/4g/8g: no trend)~~ and ~~**not the
  JIT-root path** (`--nojit` scales identically)~~ — **BOTH WITHDRAWN
  2026-08-07**, see the re-measurement at the top of this page. 1g and 2g now
  OOM, and `--nojit` is the difference between completing and exhausting the
  heap. Still ruled out: **not the young-GC livelock**, **not the STW
  cross-thread takeover** (`CRATONVM_XT_PEER_DEADLINE_MS` 1/20/200: no effect).
* **`jit_activation`'s global `Mutex` is gone** (per-thread tables since
  2026-07-31).
* **`Math.random()` is not a contention point** — a thread-local `Cell` seed
  (`native-builtins/src/lang_math.rs`), not a shared `Random`. Worth recording
  because `testConcurrentUpdate` calls it twice per update and a shared LCG is
  the obvious suspect.

## Measurement discipline this host requires

The first four are inherited; 5 and 6 are what the 2026-08-02 re-measurement had
to add after the old page's 4-thread arm turned out to be unresolvable.

1. **Never quote a debug-build ratio.** ~5-10x slower than release on its own.
2. **Never quote a multi-threaded wall-clock number.** 16 cores shared with
   15-40 sessions. Use CPU time, round-robin the arms, median or min of N, and
   record `uptime` beside every number.
3. **Aggregate `perf report` by symbol** (`--sort symbol`) — the default groups
   by command, which on a 25-thread run divides every symbol by 25 and puts
   nothing above 2.3 % — and use `--call-graph=dwarf`; the `fp` graphs resolve
   almost nothing above the leaf.
4. **An empty stdout is not a pass.** H2's `TestBase` reports some failures on
   stderr and the VM exits 1; check the exit code, not the output.
5. **Size the shape so the work term dominates the baseline.** VM start plus the
   `MERGE` seed is ~40 CPU-s with a ±15 CPU-s spread. A 4-thread × 200-update
   arm is 800 updates, about 3-6 CPU-s of work — subtracting that baseline from
   it measures the host. Use 10 000 updates at every thread count so the shapes
   are comparable to each other as well as resolvable.
6. **Interleave the 0-update baseline as an ordinary arm**, and pair the arms
   within a rep. Taken once up front, the baseline carries that minute's load
   into every number derived from it.
7. **Trust `perf`'s flat self-attribution here; do not trust its call graphs.**
   See the methodology note above — dwarf unwinding through this binary's
   inlining produces chains that are wrong, not merely shallow.
8. **Measure single-threaded too.** One thread costs nothing extra to run and
   splits every symbol into a work term and a contention term. That split is
   what reclassified `load_class_concurrent` on this page.

## Reproducing

```bash
javac -cp <h2>/target/classes -d probe probes/H2UpdateScaleProbe.java
<cratonvm> --java-home <jdk25> --Xmx 1g -c "<h2>/target/classes:probe" \
  -Dprobe.dir=./h2updb H2UpdateScaleProbe <threads> <updates> 10000
```

For the per-statement band, and for the lock-budget question the band exists to
answer:

```bash
javac -cp <h2>/target/classes -d probe \
    apps/h2database-suite-runner/probes/MergeLockBudgetProbe.java
<cratonvm> --java-home <jdk25> --Xmx 1g -c "probe:<h2>/target/classes" \
    MergeLockBudgetProbe bench 500 3          # per-statement cost
<cratonvm> ... MergeLockBudgetProbe contend 50 4 <lockTimeoutMs>
```

Run `<threads> 0 10000` for the baseline of the same shape. The full class, when
you need the real thing (~20 min of CPU):

```bash
cd <fresh writable dir>          # H2 writes ./data
<cratonvm> --java-home <jdk25> --Xmx 1g \
  -c "<h2>/target/classes:<h2>/target/test-classes:$(cat <h2>/craton-testcp.txt)" \
  org.h2.test.db.TestMultiThread
```

## Related

* the retired `bug-h2-testmultithread-concurrent-update-timeout` write-up — this
  page's predecessor: the defects that are fixed, the corrected measurement, and
  what the old numbers got wrong.
* the retired `bug-h2-testmultithread-concurrent-insert-throughput-RESOLVED-20260801`
  write-up — the INSERT half. Its flat ~25-30x across 1/2/4/8 threads is this
  page's constant factor, and its 4-thread arm was large enough to establish
  flatness where this page's was not.
* the retired `bug-h2-testtransaction-merge-using-lock-timeout-RESOLVED-20260807`
  write-up — the same wall at a **50 ms** budget instead of 10 s, where it stops
  flapping and fails deterministically. Source of this page's
  interpreter-against-interpreter table and of `MergeLockBudgetProbe`.
* `bug-h2-classid0-stale-address-family.md` — the memory-safety family
  found in this class. Unrelated to throughput.
* the retired `bug-h2-mvstore-insert-loop-perf-hang` write-up
  (`docs/internal/fixed-suite-bugs/h2-suite-bugs/…-RESOLVED-20260807.md`) —
  source of the INSERT table and the flat profile above, plus the two mark-word
  quartet defects found while reproducing it.
