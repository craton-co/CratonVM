# CLOSED — `ResourceLeakDetectorTest.testConcurrentUsage` is slow, not hung; and the reason G1 was 14x worse than ZGC was one line

**Status:** ✅ CLOSED 2026-08-17 on `fix/netty-sni-ocsp-rld-residuals-20260817`.
Retires `docs/known-issues/netty/resourceleakdetector-concurrentusage-timeout-20260815.md`.

That page asked four questions and warned that its own `@Timeout` hid the first
one. All four are answered, and chasing them found a real, general defect in
G1's allocation path that had nothing to do with references.

## 1. Slow or hung? — SLOW. It passes in 161 s.

The page's own instruction was "raise the budget before measuring anything
else", and noted that `-Djunit.jupiter.execution.timeout.default` cannot
override a method-level `@Timeout`. It can't — but
`-Djunit.jupiter.execution.timeout.mode=disabled` can, and needs no edited
fixture. Same class, same classpath, per-method wall time:

| arm | `testConcurrentUsage` | `testLeakBrokenHint` | `testLeakSetupHints` | class |
|---|---|---|---|---|
| CratonVM ZGC, this branch | **161.0 s SUCCESSFUL** | 12.1 s ok | 0.3 s ok | 177.7 s, 3/3 |
| HotSpot 25 | 5.3 s ok | 6.6 s **FAILED** | 3.2 s **FAILED** | 9.2 s, 1/3 |

So: it is not a hang, it is not a correctness defect, and the only thing that
ever failed was the 60 s budget. CratonVM passes all three; HotSpot fails two,
for the leak-report cross-contamination the page already identified as not
ours. The page's warning — "a passing test whose oracle fails is a question,
not a credit" — resolves in CratonVM's favour here: with the clock removed,
this VM's answers are the correct ones on all three.

30x on the method (161 s vs 5.3 s) for 5,000,000 tracked objects.

## 2. Is it the collector? — Yes, and that was a real defect (now fixed)

The page recorded ZGC and G1 as indistinguishable (61-65 s vs 61-63 s) and
concluded "it is not the collector". That reading was an artifact of the
`@Timeout`: both arms were cut off at 60 s, so both reported ~60 s. Measured
with a standalone replica of the same workload at 1/100 scale (50 threads,
50 000 tracked), interleaved ABBA, four runs per arm:

| collector | control | this branch |
|---|---|---|
| ZGC | 1.70 s, 1.62 s | 1.72 s, 1.68 s |
| G1 | 41.5 s, 28.5 s | 24.4 s, 24.0 s |

G1 was **~20x slower than ZGC on the identical workload**. Two collectors that
looked equal through a timeout were an order of magnitude apart.

### Root cause: `G1::needs_gc()` was O(regions) under the global regions lock, per allocation

```rust
fn needs_gc(&self) -> bool {
    let regions = self.regions.lock();
    let free_count = regions.iter()
        .filter(|r| r.region_type == RegionType::Free).count();
    ...
}
```

`maybe_gc` calls `needs_gc()` after **every** `new` / `newarray` / `anewarray`
bytecode. So every allocated object paid a 1500-iteration scan (1 MiB regions
at `-Xmx1500m`) while holding the one mutex every allocation path also needs.
Single-threaded that is ~1 µs of pure overhead per object; with several
mutators it is a convoy.

The file already stated the correct discipline one function above, on
`note_region_consumed_locked`: *"Called ONLY when a new region is consumed —
once per `region_size` bytes of allocation, never per object — so the
O(num_regions) count is amortized away."* `needs_gc` violated exactly that.

Isolated with a constant-total-work allocation probe (`ScaleProbe alloc`,
1 000 000 objects, total work fixed as thread count rises):

| threads | G1 control | G1 fixed | ZGC (control, for scale) |
|---|---|---|---|
| 1 | 5086 ms | **889 ms** | 1209 ms |
| 8 | 19 574 ms | **1416 ms** | 1185 ms |
| 50 | ~30 800 ms | **9767 ms** | — |

**13.8x at eight threads**, and G1 now tracks ZGC instead of diverging from it.
The fix is an atomic cached Free count, published wherever the count is already
being taken under the lock (`note_region_consumed_locked`, the TLAB refill's
own reserve count, `with_regions_mut`, and the end of `collect_garbage`), with
a bounded re-scan every `NEEDS_GC_RECOUNT_INTERVAL` = 1024 queries as a
self-healing backstop for any region-type transition that does neither. Two
unit tests pin both directions — a full heap must ask for a GC, and an
all-Free heap must not keep asking.

### Second, smaller G1 finding: the rset add on a repeated edge

With `needs_gc` fixed, a workload that RETAINS what it allocates still showed a
G1-only thread penalty (`ScaleProbe allockeep`, 1 M stores into an `Object[]`:
1.0 s on one thread, 6.8 s on eight, with **zero collections in either run** —
so pauses were never involved). That is the reference-store barrier:
`post_write_barrier_rset` takes a per-destination-region
`Mutex<FxHashMap>` on every cross-region store, and mutators writing into the
same current Eden region all contend on that one mutex.

An rset is a SET of source region indices, so re-recording an edge this thread
already recorded is a no-op — discovering that by taking the lock is the whole
cost. A one-slot thread-local memo of the last
`(collector_id, dst_idx, src_idx, epoch)` skips it. Soundness rests on the same
`rset_cache_epoch` the existing region-pointer cache already relies on: a
recycle/retype phase bumps it under the regions lock, and `G1Region::reset`
(which clears the rset) only happens inside such a phase, so a stale memo
cannot survive the clear that would invalidate it. ABBA, 1 M retained stores,
8 threads: control 31.1 s / 15.9 s, fixed 7.3 s / 10.1 s — ~2.7x, on a noisy
host.

### Third: `ReferenceQueue.poll()` took the queue's monitor to answer "empty"

`ResourceLeakDetector.track()` polls its shared queue once per tracked object,
so one queue object was the contention point for every mutator, for a call that
returns null essentially every time. A null head is now answered by an unlocked
read; the pop still runs under the monitor and re-reads `head` there. Racing
the read is sound in a way racing the pop is not — `head == null` means
"nothing enqueued as of this read", and the JDK's own `poll` answers null too
whenever it wins its lock first. Worth ~3 µs per empty poll on the microbench;
small on this test.

## 3. Is it the reference processor, the thread count, the barrier, or the JIT?

The page listed four candidates and said none had been separated. Separated, at
constant total work, 8 threads, 400 000 iterations (`RefCostMt`):

| primitive | ZGC | G1 control | HotSpot |
|---|---|---|---|
| `new Object()` + retain | 1.5 µs | 39.2 µs | 0.1 µs |
| `+ new WeakReference(o, q)` | 3.4 µs | 64.7 µs | 0.12 µs |
| `+ q.poll()` | 4.9 µs | 86.3 µs | 0.12 µs |
| `+ CHM put/remove + clear` | 13.1 µs | 151.9 µs | 0.46 µs |

* **Thread count** was the dominant term and it was the G1 defect above, not
  the `ref_processor` L7 mutex the page suspected. On ZGC the same work is FLAT
  from 1 to 8 threads.
* **The reference processor** is not the hot term: the WeakReference
  construction adds ~1.9 µs on ZGC, against the ~13 µs the whole
  track/close cycle costs.
* **The JIT** finding in the page ("ZGC with the JIT and ZGC with `--nojit` are
  the same to within noise… itself the finding worth chasing first") was a red
  herring, and cheaply so: the same equality holds on the pure-allocation probe
  (G1, 1 M objects, 8 threads: 13.7 s with the JIT, 17.9 s with `--nojit`), so
  it says the cost is not in compiled Java bodies at all — it was in the
  collector call the allocation makes. Chasing JIT reach first would have found
  nothing.
* **What is left** after all three fixes is per-operation cost on the
  `WeakReference` + `ConcurrentHashMap` path: ~13 µs against HotSpot's 0.46 µs.
  That is this VM's general interpreted/native throughput, not a defect in this
  test, and it is why the method still needs ~161 s where HotSpot needs 5.3 s.

## Where this leaves the test

`testConcurrentUsage` still exceeds its own `@Timeout(60000)` and will keep
being recorded as FAIL by the suite. That is the honest state: the test asks for
5 000 000 tracked `WeakReference`s in under a minute, and closing the remaining
30x is a throughput programme, not a bug fix. Deliberately NOT given a
`class-overrides.tsv` floor — the runner's floor raises the harness's wall cap
and cannot touch a method-level `@Timeout`, so an override here would buy
nothing and only hide the row.

What the page asked for is done: the hang/slow question is answered with a
passing 161 s run, the collector question is answered and the collector defect
behind it is fixed, and the four candidate costs are separated with numbers.

## Reproducing

```bash
cd apps/netty-suite-runner
# the question the @Timeout hid — needs `mode=disabled`, not `timeout.default`
<cratonvm> --java-home <jdk-25> --Xmx 1500m -XX:+UseZGC \
  -cp "<probe-dir>;<netty cp>" -Duser.timezone=UTC \
  -Djunit.jupiter.execution.timeout.mode=disabled \
  PerTestTimer io.netty.util.ResourceLeakDetectorTest
```

`PerTestTimer`, `RldConcurrentProbe`, `RefCostMt` and `ScaleProbe` live in
`apps/netty-suite-runner/probe-sniocsp/` (gitignored with the rest of `apps/`).

## Related

- `sniclienttest-sni-refusal-alert-FIXED-20260817.md`,
  `ocspclienttest-is-sixteen-rsa-certificates-CLOSED-20260817.md` — the two
  sibling pages retired in the same pass.
- `docs/known-issues/g1-retained-store-thread-scaling-20260817.md` — the
  residue of the write-barrier finding above, filed with its measurement.
