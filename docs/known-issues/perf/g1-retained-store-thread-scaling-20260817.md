# G1 loses ~7x when the same reference stores are spread over more threads

**Status: OPEN**, characterised 2026-08-17 on
`fix/netty-sni-ocsp-rld-residuals-20260817`, Windows host, `cratonvm.exe`
release build, 8 physical cores. Split out of the
`ResourceLeakDetectorTest.testConcurrentUsage` investigation, which fixed two
larger G1 defects on the way here and left this one standing.

## What it is

Fixed total work, rising thread count, one small object per iteration stored
into a local `Object[]` (`ScaleProbe allockeep`, 1 000 000 objects):

| threads | G1 | ZGC |
|---|---|---|
| 1 | 1.0 s | 1.09 s |
| 8 | 6.8 s | 1.15 s |

ZGC is flat. G1 costs ~7x more to do the same work on eight cores than on one.

The two obvious explanations are both ruled out:

* **Not the pauses.** `CRATONVM_DBG_G1DIAG=1` reports **zero collections** in
  both the 1-thread and the 8-thread run. 1 M 16-byte objects is 16 MiB against
  a 1500 MiB heap.
* **Not the allocation trigger.** That was a real defect in the same family and
  it is fixed on this branch (`G1::needs_gc` was an O(num_regions) scan under
  the global regions lock, called after every `new` bytecode — see the retired
  `resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817`
  page). With it fixed, the drop-the-object variant of the same probe
  (`ScaleProbe alloc`) is 0.89 s on one thread and 1.42 s on eight. Retention
  is what reintroduces the penalty, so the cost is on the STORE, not the
  allocation.

The store is therefore the suspect: an `aastore` of a reference goes through
`G1Collector::satb_pre_barrier` + `write_barrier` →
`post_write_barrier_rset`, none of which ZGC has an equivalent of on this path.

## What has already been tried

`post_write_barrier_rset` takes a per-destination-region
`Mutex<FxHashMap>` (`RememberedSet::add_reference_in_generation`) on every
cross-region store, and every mutator in this probe is storing into the same
current Eden region — so the obvious hypothesis was a convoy on that one mutex.
A thread-local memo of the last `(collector_id, dst_idx, src_idx, epoch)` edge,
which lets a repeated edge skip the lock entirely (an rset is a SET, so
re-adding is a no-op), landed on this branch and is worth about **2.7x** on this
probe — real, but it does not close the gap, and it does not explain the
remaining ~2.5x.

So the mutex was part of it and is not the whole story. Whatever is left is
still inside the barrier pair, or in the `aastore` path's interaction with it.

## Candidate costs, none separated yet

* `lookup_region_for_addr` runs TWICE per store (src and dst). It is a binary
  search over an immutable 1500-entry table — no lock, ~11 random accesses into
  24 KB, so nominally ~100 ns for the pair. Cheap in theory; not measured under
  8-thread cache pressure, where the table is shared read-mostly across cores.
* The `LAST_RSET_TARGET` thread-local region-pointer cache misses whenever
  `dst_idx` changes, and Eden advances every ~62 k stores in this probe. A miss
  takes `regions.lock()`. That is ~16 lock acquisitions per million stores, so
  it should not matter — unless the cache is missing far more often than the
  Eden-advance rate, which nobody has counted.
* `satb_pre_barrier` reads the OLD slot value before the store. On the
  `Object[]` path that read goes through the array element accessor; whether it
  is doing more than a load has not been checked.
* `rset_cache_epoch` is an `Acquire` load on every store — a shared cache line
  read by 8 cores. Cheap per access, but it is on the very hottest path.

## The instrument this needs

None of the above can be separated from Java-level timing, and
`--stack-sample-ms` cannot see it: the cost is in Rust, and the Java sampler
reports only the invoking bytecode. What would answer it in one run is a set of
per-barrier counters (fast-path hit, edge-memo hit, region-pointer-cache miss,
lock acquisitions) dumped at exit behind one debug flag — the same shape as the
TLAB hit/refill counters that already exist but have no output path either.

## Why it matters beyond this probe

`ResourceLeakDetectorTest.testConcurrentUsage` is 24 s on G1 against 1.7 s on
ZGC for the same 50 000 tracked objects after this branch's fixes — a 14x
collector gap on a workload that allocates and retains reference-carrying
objects from 50 threads, which is a shape a lot of the netty suite has. Two
collectors an order of magnitude apart on the same workload also makes every
G1-arm timing in the suite tables hard to read.

## Repro

```bash
cd apps/netty-suite-runner
for t in 1 8; do
  <cratonvm> --java-home <jdk-25> --Xmx 1500m -XX:+UseG1GC \
    -cp "<probe-dir>" ScaleProbe allockeep $t 1000000
done
# and the same with -XX:+UseZGC for the control arm
```

`ScaleProbe`, `RefCostMt` and `RldConcurrentProbe` are in
`apps/netty-suite-runner/probe-sniocsp/` (gitignored with the rest of `apps/`).
Interleave the arms — this host swings a factor of two on a 30 s measurement.

## Related

- `resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817` —
  the retired page this was split out of, with the two G1 fixes that landed and
  the per-primitive cost table.
- `biginteger-modpow-has-no-montgomery-reduction-20260817.md` — the other
  residue filed out of the same pass.
