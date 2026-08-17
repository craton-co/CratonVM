# G1's non-null reference store still scales ~4x with thread count — and it is none of the things it looked like

**Status: OPEN, narrowed.** Opened 2026-08-17 out of the
`ResourceLeakDetectorTest.testConcurrentUsage` investigation. Three of the
causes originally named on this page have since been found and fixed on
`perf/g1-retained-store-barrier-20260817`; what remains is the residue plus the
list of candidates that measurement has RULED OUT. Windows host, 8 physical
cores, `cratonvm.exe` release build.

## The residue, isolated

Constant total work, pre-allocated arrays, no allocation and no GC inside the
timed loop, ordinary (non-humongous) arrays — `BarrierProbe`, 2M ops, the same
binary for both collectors:

| mode | what the loop does | G1 1thr | G1 8thr | ZGC 1thr | ZGC 8thr |
|---|---|---|---|---|---|
| `primstore` | `int[i] = k` | 707 | 735 | 634 | 758 |
| `refnull` | `Object[i] = null` | 707 | 735 | 634 | 758 |
| `refread` | reads `Object[i]`, no store | 711 | 759 | 645 | 697 |
| `refself` | `Object[i] = theArray` | 990 | **3405** | 829 | 852 |
| `refstore` | `Object[i] = other[i]` | 1100 | **5501** | 953 | 754 |

Everything is flat except the two modes that store a **non-null reference**,
and both of those are flat on ZGC. So the residue is exactly the non-null arm
of G1's post-write barrier: ~1.5 µs per store at eight threads against ~0.2 µs
at one.

The sharpest version of the fact: `refnull` and `refself` make the **same**
number of accessor calls (6 167 561 vs 6 167 598), both at
`lock_free_pct=100.00`, and differ 5x in wall time. The only difference between
those two loops is whether the stored reference is null.

## What has been RULED OUT, and by what

Each of these was plausible; two were this page's own original guesses. Recorded
so nobody re-runs them.

* **The remembered-set mutex.** `RememberedSet::add_reference_in_generation`
  takes a per-region `Mutex<FxHashMap>`, and mutators storing into one hot
  region all contend on it. That was the original hypothesis. The
  `CRATONVM_DBG_G1ACCESSOR` census settles it — 2M-store `refstore`, eight
  threads:

  ```
  rset cross_region_stores=226283 memo_hit=226256 took_rset_mutex=27 \
    same_region_skipped=1839949 memo_hit_pct=99.99
  ```

  **27 mutex acquisitions in the entire run.** The four-slot edge memo is doing
  its job; the mutex is not the cost.

* **The accessor lock.** It WAS a cost, an enormous one, and it is fixed — see
  below. But it is not THIS: with it gone, `primstore` and `refnull` went flat
  while `refself` did not, and the census reports `took_regions_lock=0` for
  every mode in the table.

* **The region lookup.** `lookup_region_for_addr` was a `partition_point` over
  1500 entries, called twice per store. Now O(1) arithmetic (the arena is one
  contiguous run of equal adjacent slices). Worth ~7-10% at one thread and
  nothing at eight. Not the cost.

* **The SATB pre-barrier.** `pairref` stores null and then a non-null reference
  into the same slot, so the pre-barrier is only ever handed a NULL old value;
  `pairnull` does the same two stores with both null. `pairnull` is flat
  (370 → 404) and `pairref` scales 3.8x (593 → 2225). Neither ever feeds the
  pre-barrier a non-null old value, so it is not what scales.

* **The array read.** `refread` — 2M non-null `aaload`s, which route through
  `autobox_payload` → `is_object_address` → `is_addr_in_live_region`, and that
  last one DOES take the regions lock after its arena gate — is flat, 711 → 759.

* **Anything collector-independent.** The interpreter's `aastore` handler runs a
  covariance check, three `Arc::clone`s and two `to_string()`s that a null store
  skips. All identical on both collectors, and ZGC is flat. (Those `to_string`s
  and `Arc::clone`s per array store are worth a look on their own account — but
  they are not this.)

* **GC pauses.** `CRATONVM_DBG_G1DIAG=1` reports **zero collections** in these
  runs.

## What was found and fixed on the way here

Three real defects, all landed on the branch above:

1. **Every field and array accessor took the global `regions` lock**, purely to
   ask `humongous_span` whether the object was humongous — so `getfield`,
   `putfield`, `aaload` and `aastore` all serialized against each other across
   threads. Fixed with `may_be_humongous`, a lock-free size test exact against
   the allocator's own admission rule (`size > region_size / 2`). This is what
   made `primstore` — an `int[]` store, no GC barrier of any kind — scale 4.1x.
   Now flat; 5.5-6.3x faster at eight threads.
2. **The rset edge memo was one slot and thrashed.** A store loop into one
   destination array reaches objects in several source regions, so a
   single-entry memo alternates between them and misses every time. Four slots,
   `(dst, src)` packed into one `u64` so the whole memo is 32 bytes to copy.
3. **`lookup_region_for_addr` was O(log R)** where the contiguous arena makes it
   arithmetic.

## Where to take it next

Everything left is inside the non-null arm between `set_array_element`'s tail
and `post_write_barrier_rset`'s early return — which, after the three fixes
above, is a few instructions of arithmetic. That mismatch IS the finding: a
handful of instructions costing 1.5 µs under eight threads says the cost is not
in the instructions but in what they touch, i.e. a shared cache line. The
candidates are `self.instance_id`, the `rset_cache_epoch` Acquire load, and the
call site's own register/spill traffic when the branch is taken.

**This has run out of what Java-level bisection can answer.** Every remaining
hypothesis costs a 15-minute VM build to test, and the last three were all
wrong. The next instrument should be a native profiler on the mutator threads —
`perf record` on the Azure Linux host, which has it installed and where this
workload reproduces with no network fixture — not another counter.

## Repro

```bash
cd apps/netty-suite-runner
for m in primstore refnull refread refself refstore pairnull pairref; do
  for t in 1 8; do
    <cratonvm> --java-home <jdk-25> --Xmx 1500m -XX:+UseG1GC \
      -cp "<probe-dir>" BarrierProbe $m $t 2000000 4096
  done
done
# same with -XX:+UseZGC for the control arm
# CRATONVM_DBG_G1ACCESSOR=1 adds the lock/memo census at exit
```

`BarrierProbe` is in `apps/netty-suite-runner/probe-g1barrier/` (gitignored with
the rest of `apps/`).

**The fourth argument matters.** An `Object[1<<16]` is 524 328 bytes, over a
1 MiB region's `region_size / 2` humongous threshold, so sizing the probe there
measures the humongous path — which correctly still takes the lock — instead of
the ordinary one. That mis-sizing hid the accessor fix's effect entirely on the
first measurement pass.

**Interleave the arms.** This box swings a factor of two on a 30-second
measurement; one 768 ms figure re-measured as 4630 ms while another session was
building (51 concurrent `rustc` processes).

## Related

- `resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817` —
  the retired page this was split out of, with the two G1 fixes that landed and
  the per-primitive cost table.
- `../../internal/performance/biginteger-modpow-montgomery-FIXED-20260817.md`
  (FIXED 2026-08-17) — the other residue filed out of the same pass, since
  fixed by someone else.
