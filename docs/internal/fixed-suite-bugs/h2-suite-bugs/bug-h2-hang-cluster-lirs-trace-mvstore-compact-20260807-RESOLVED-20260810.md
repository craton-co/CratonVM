# The three "HANGs outside the row-iteration cliff" — none of them is stuck (RESOLVED 2026-08-10)

## Status
**RESOLVED / retired 2026-08-10.** Was
`docs/known-issues/h2/bug-h2-hang-cluster-lirs-trace-mvstore-compact-20260807.md`
(OPEN, single-sample, 2026-08-07). The page's own open question —
"progressing but slow" vs "stuck", which one dump each could not answer — is
answered for all three classes, and its three "next steps" are all run.

Nothing here is a defect with a fix. Two classes are the general per-call cost
(now quantified, and handed to the page that owns it); the third never
terminates on any JVM.

## Method
Per-PROCESS CPU (`/usr/bin/time`), not wall clock — the Azure host runs at load
9-17 with other work on it, so wall time is not comparable between arms and CPU
seconds are. HotSpot's own `-Xint` is included because it, not C2, is the
control for an interpreter: the CratonVM-vs-C2 column is dominated by what C2
does, and moves with host load.

`--stack-sample-ms 250` gives one dump per interval per running thread, which is
what settles stuck-vs-progressing: a stuck thread repeats one leaf, a
progressing one does not.

| class | HotSpot C2 | HotSpot `-Xint` | CratonVM JIT | CratonVM `--nojit` | nojit / -Xint | rc |
| --- | --- | --- | --- | --- | --- | --- |
| `TestLIRSMemoryConsumption` | 13.2 s | 51.5 s | 554.0 s | 1475.9 s | **28.6x** | 0 |
| `TestBtreeIndex` | 7.7 s | 47.2 s | 569.3 s | 1187.5 s | **25.1x** | 0 |
| `TestSynth` | *never terminates* | — | — | — | — | 124 |

Both of the first two **PASS** (`rc=0`). They exceed the suite's 300 s per-class
cap; they do not hang.

## 1. `TestLIRSMemoryConsumption` — progressing, and not in `addToQueue`

77 samples over the run, **8 distinct leaves**, frames varying throughout:

| share | leaf |
| --- | --- |
| 57.1% | `org/h2/util/Utils.collectGarbage` |
| 24.7% | `CacheLongKeyLIRS$Segment.<init>` |
| 11.7% | `CacheLongKeyLIRS$Entry.<init>` |
| 1.3% each | `Segment.evictBlock`, `Segment.removeFromQueue`, `Segment.get`, `Entry.getValue`, `Segment.addToStack` |

The page's single dump caught `evict` → `evictBlock` → `addToQueue`, i.e. the
1.3% tail. Its "possible genuine algorithmic loop" reading is **refuted**: the
eviction bookkeeping is a rounding error in the profile, and the class
completes.

The work is real and large: `testMemoryConsumption` is run three times, each
over five cache sizes, each doing 1 000 000 `put`s and 1 000 000 random
`get`-or-`put`s — **30 million cache operations**, plus a full GC per size from
H2's own `Utils.collectGarbage()`. That GC is over half the sampled time.

The page's "same general family" reading is the right one, with a correction:
at **28.6x interpreter-to-interpreter** this class is ~3x the flat 9.9-10.7x
band the throughput pages measured, not inside it. §4 says why.

*Found in passing, FIXED 2026-08-11:* `Runtime.totalMemory()` and
`freeMemory()` were hardcoded constants (64 MiB / 32 MiB) that never moved — not
on allocation, not across `Runtime.gc()` — so every memory delta this class
printed was `0` where HotSpot prints real numbers. They now report the real
committed heap and committed-minus-used (`VmHeap::committed_bytes`, across all
three collectors). This class's second column, per cache size:

| cache MiB | 1 | 2 | 4 | 8 | 16 |
| --- | --- | --- | --- | --- | --- |
| before | 0 | 0 | 0 | 0 | 0 |
| after | 1 | 3 | 6 | 11 | 23 |
| HotSpot 25 | 1 | 2 | 5 | 9 | 19 |

It cost nothing: ABBA-interleaved on this class, per-PROCESS CPU, 359.8 / 348.9 s
before against 352.7 / 358.3 s after — +0.3% on the means, all four `rc=0`. The
risk worth checking first was that `Utils.getMemoryUsed()` calls
`Utils.collectGarbage()`, which in some H2 versions loops `runtime.gc()` until
`totalMemory()` stops changing: a value that tracked occupancy would have turned
one collection into a fixed run of full ones. **This H2's `collectGarbage()`
polls the JMX collection COUNT, not `totalMemory()`** — so the coupling does not
exist here, and `committed_bytes` is a capacity by construction, so it would not
fire where it does.

## 2. `TestBtreeIndex` — progressing, and `Trace.isEnabled` is not special

2643 samples, **183 distinct leaves**. `Trace.isEnabled` is there, and it is
third, not dominant:

| share | leaf |
| --- | --- |
| 18.2% | `org/h2/mvstore/db/ValueDataType.write` |
| 14.6% | `org/h2/mvstore/FileStore.readPage` |
| **8.7%** | **`org/h2/message/Trace.isEnabled`** |
| 6.2% | `JdbcResultSet.nextRow` |
| 5.6% | `JdbcResultSet.getIntInternal` |
| 4.5% | `MVMap.operate` |
| … 177 more, none above 4% | |

The page asked for the per-call cost of that gate "in isolation, on CratonVM vs
HotSpot". `TraceBench.java` measures it — `Trace.isDebugEnabled()`, which is
`isEnabled(3)` → an `invokeinterface` to `TraceSystem.isEnabled` when
`traceLevel` is `PARENT` — with two receivers giving different answers and the
result summed into a `volatile` sink, because a first attempt with one
always-false receiver measured **0.00 ns/call on HotSpot C2**: it measured
dead-code elimination, not the gate.

| arm | ns per `isDebugEnabled()` |
| --- | --- |
| HotSpot C2 | 1.16 |
| HotSpot `-Xint` | 57 – 72 |
| CratonVM `--nojit` | 2560 – 2990 |
| CratonVM JIT | 2060 – 3180 |

~**42x** HotSpot's own interpreter. Two things it is NOT: it is not a native
call, and it is not a JIT dispatch pathology — the JIT arm is within noise of
`--nojit`, so nothing here is the "inline cache never publishes" family.

## 3. `TestSynth` — read the fixture

```java
@Override
public void test() throws Exception {
    while (true) {
        int seed = MathUtils.randomInt(Integer.MAX_VALUE);
        testCase(seed);
    }
}
```

There is no termination condition. `TestSynth` is an endless random-SQL fuzzer;
it "HANGs at the per-class timeout" on every JVM, by construction. HotSpot 25
was given 2400 s — eight times the suite's own 300 s cap — and hit the cap
(`rc=124`, 438 CPU-s), still printing new seeds.

So the page's third entry has no CratonVM-side content at all. The
`Database.close → MVStore.closeStore → FileStore.compactStore →
RandomAccessStore.compactStore` chain in its dump is one fuzz iteration's
close path, sampled somewhere inside an unbounded loop — H2 closes and deletes
the database once per seed (`testCase` starts with `deleteDb`). Its "next step"
— reproduce `FileStore.compact()` standalone against a pre-fragmented store —
is not worth doing on this evidence: there is no reason to believe compaction
was slow, only that a sample landed in it.

## 4. What the 25-29x actually is: the per-call rung

Both surviving classes are call-dense, and both sit ~3x above the flat
interpreter band. `CallShapeBench.java` isolates why. It rebuilds `Trace`'s
exact shape out of synthetic classes — bimorphic receiver → `invokevirtual`
`isDebugEnabled` → `invokevirtual` `isEnabled` → `invokeinterface`
`W.isEnabled`, four field loads, no allocation — and subtracts an
otherwise-identical loop that makes no calls:

| arm | 3-call chain, net of the loop | loop only (`ts[i&1] != null`) |
| --- | --- | --- |
| HotSpot `-Xint` | 46 – 49 ns | 6 – 11 ns/iter |
| CratonVM `--nojit` | 1756 – 2231 ns | 125 – 153 ns/iter |
| ratio | **~40-48x** | **~13-21x** |

Interleaved arms, same host, load 11-12; the numbers repeat across runs.

Two readings fall out:

* **H2's `Trace` is not a discrete inefficiency.** The synthetic control costs
  what `Trace.isDebugEnabled()` costs. Nothing about that class needs looking
  at; the page's "worth checking whether `Trace.isEnabled` bottoms out in a
  native call, a volatile/atomic read, or something heavier" is answered — no.
* **A call level costs ~700 ns interpreted, against ~15 ns on HotSpot `-Xint`.**
  The call-free loop is ~13-21x — the ordinary band — so the whole excess is in
  the calls. That is the mechanism that turns a 10x class into a 25-29x class,
  and it is the only actionable number this page produced.

That number belongs to the page that owns the throughput gap, not here: it is
recorded on `performance/h2-update-path-throughput-RETIRED-20260821.md`, which
stays open.

## Reproducing

```bash
cd apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
# per-process CPU, no watchdog, generous cap
/usr/bin/time -f "wall=%e user=%U sys=%S rc=%x" timeout 2400 \
  <cratonvm-bin> --java-home $JDK25 --Xmx 2g --stack-sample-ms 250 -c "$CP" <class>
```

Aggregate the deepest frame of each `--- T19.H1 end dump ---` block to get the
leaf histogram. `probes/TraceBench.java` and `probes/CallShapeBench.java` are
the two microbenches; run each against `java -Xint` on the same host in the
same window, not against C2.

## Related
* `../../performance/h2-update-path-throughput-RETIRED-20260821.md` — owns the
  constant factor, and now carries the per-call rung above.
* `bug-h2-mvstore-insert-loop-perf-hang-RESOLVED-20260807.md` (this folder) —
  the row-iteration/commit cliff this page was split out of. Its flat 4%-max
  profile and its "quote the interpreter-against-interpreter ratio" rule are
  the method used here.
