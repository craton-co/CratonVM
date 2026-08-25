# G1's non-null reference store — the thread scaling is GONE, and what was left of it was never G1's

**Status: RETIRED 2026-08-24.** The residue this page was opened for does not
exist any more, on either platform, and the one live cost it flagged in passing
turned out to be the whole of the remaining non-null penalty — collector-
independent, in the interpreter's array-store covariance check, and now fixed
(1.5–1.9×).

Opened 2026-08-17 out of the `ResourceLeakDetectorTest.testConcurrentUsage`
investigation, on a Windows host with 8 physical cores. Its claim was that
G1's post-write barrier scaled ~4× with thread count on the **non-null** arm
only, while ZGC stayed flat — i.e. a G1-specific defect, isolated to the
handful of instructions between `set_array_element`'s tail and
`post_write_barrier_rset`'s early return, and therefore attributed to a shared
cache line. The page's own next step was: *"the next instrument should be a
native profiler on the mutator threads — `perf record` on the Azure Linux
host, where this workload reproduces with no network fixture."*

## 1. It does not reproduce — and the control is the collector, not the clock

`probes/BarrierProbe` was rewritten from this page's own description (the
original lived only in a gitignored `apps/` directory and is gone); it is now
checked in at `probes/BarrierProbe.java`, with a fifth argument selecting
shared vs per-thread arrays. Total work is CONSTANT: `ops` stores split evenly
over `threads`, every array pre-allocated and pre-filled, nothing allocated
inside the timed region.

**Every configuration below was run ABBA-interleaved with its own control and
reported as the MINIMUM of five rounds.** Both hosts are shared and swing a
factor of two on a 30-second measurement, which is precisely why the load-proof
comparison here is **G1 against ZGC in the same round on the same binary**, not
either against a remembered number.

**Linux, 8 cores, `dev` @ `2e9286dde`, 4M stores, min of 5:**

| mode | G1 1thr | G1 8thr | ZGC 1thr | ZGC 8thr |
|---|---:|---:|---:|---:|
| `primstore` | 560 | 224 | 547 | 246 |
| `refnull` | 614 | 318 | 608 | 357 |
| `refread` | 758 | 411 | 768 | 433 |
| `refself` | 1335 | **711** | 1296 | **872** |
| `refstore` | 1713 | **925** | 1833 | **969** |
| `pairnull` | 1047 | 602 | 1137 | 835 |
| `pairref` | 2191 | 1337 | 2155 | 1091 |

**Windows, 8 physical cores — the original host — `cratonvm.exe` at
`target/release`, 2M stores, min of 5:**

| mode | G1 1thr | G1 8thr | G1 speedup | ZGC 1thr | ZGC 8thr | ZGC speedup |
|---|---:|---:|---:|---:|---:|---:|
| `primstore` | 1305 | 313 | 4.2× | 1271 | 367 | 3.5× |
| `refnull` | 1389 | 382 | 3.6× | 1683 | 346 | 4.9× |
| `refread` | 2328 | 597 | 3.9× | 2580 | 524 | 4.9× |
| `refself` | 3645 | **944** | 3.9× | 5115 | **938** | 5.5× |
| `refstore` | 6010 | **1146** | 5.2× | 5834 | **1206** | 4.8× |
| `pairnull` | 3520 | 642 | 5.5× | 2436 | 489 | 5.0× |
| `pairref` | 5217 | 1450 | 3.6× | 4338 | 1202 | 3.6× |

Read the two bold rows. This page's table had G1 `refself` going **990 → 3405**
and G1 `refstore` **1100 → 5501** while ZGC stayed at 852 / 754 — negative
scaling on one collector and flat on the other. Today every mode on both
collectors scales **positively, by 3.5–5.5× on eight cores**, and the two
collectors land on top of each other in the arms that were supposed to be 4×
apart: `refself` 944 vs 938, `refstore` 1146 vs 1206. On Linux G1 is
*faster* than ZGC in both.

There is no G1-specific non-null residue left to profile. The three fixes this
page records as landed — the global `regions` lock behind `may_be_humongous`,
the four-slot rset edge memo, and the O(1) `lookup_region_for_addr` — are
between them what closed it; the page was written believing a residue survived
them, and on a quiet host measured against its own collector control, it does
not.

## 2. What WAS still real: the non-null arm costs ~2.5× the null one, on BOTH collectors

The page noticed this and set it aside:

> The interpreter's `aastore` handler runs a covariance check, three
> `Arc::clone`s and two `to_string()`s that a null store skips. All identical on
> both collectors, and ZGC is flat. (Those `to_string`s and `Arc::clone`s per
> array store are worth a look on their own account — but they are not this.)

With the G1 residue gone, that IS the remaining fact, and it is large. One
thread, min of five: Linux `refself` 1335 against `refnull` 614; Windows 3645
against 1389; ZGC 1296/608 and 5115/1683. **The same ~2.4–3.0× on both
collectors** — which is exactly what says it is not a collector cost at all.

`refnull` and `refself` differ inside the interpreter by two things: the JVMS
covariance check, and `post_write_barrier_rset`. ZGC's post-write barrier does
nothing, and ZGC shows the same ratio, so it is the covariance check.

### The two costs, and the fix

**`aastore_element_assignable` built two `String`s to answer a question about
one `ClassId`.** It opened with `array_descriptor_of`, which on a reference
array takes the class-manager read lock, does `c.name.to_string()`, and then
`format!("[L{};", comp_name)` — so that forty lines later it can compare the
result against the literal `"Ljava/lang/Object;"` and return `true`. `Object[]`
is the single commonest reference-array shape there is (`ArrayList.elementData`,
every varargs pack, every `toArray()`), so that arm carries most of the traffic
and never uses the string it built.

Replaced by `reference_array_component_is_object`, which reads the component
`ClassId` out of the header and compares the class's name under a one-slot
per-thread memo keyed by `(vm identity, ClassId)`. The VM identity is in the key
because `ClassId`s are per-VM dense indices and the test harness builds several
`SharedVm`s in one process. The two forms agree on every input: an unnameable
component answers `true` in both, because `array_descriptor_of` substitutes
`[Ljava/lang/Object;` for it.

**All four array-store opcode handlers materialised the class and method NAME
on every store**, as `_diag_class` / `_diag_method`, for a message only ever
built when the array reference turns out to be null. They are resolved inside
the closure now, from the `ClassId` and the method-name `Arc` the same site
already had to capture for the JEP 358 arm. (Hoisting them was not a mistake to
begin with — the closure cannot borrow `thread` while
`thread.frames[..].stack` is mutably borrowed by `pop_object_ref_ctx_with`. A
`ClassId` needs no such borrow, which is what makes the lazy form possible.)

### Measured

Same binary pair, ABBA-interleaved, four rounds, minimum of eight samples per
cell, 4M stores, Linux:

| mode | G1 before | G1 after | ratio | ZGC before | ZGC after | ratio |
|---|---:|---:|---:|---:|---:|---:|
| `refnull` 1thr — **control** | 691 | 664 | 1.04 | 700 | 637 | 1.10 |
| `refnull` 8thr — **control** | 708 | 722 | 0.98 | 648 | 746 | 0.87 |
| `refself` 1thr | 1719 | **924** | **1.86** | 1717 | **909** | **1.89** |
| `refself` 8thr | 1408 | 1158 | 1.22 | 1387 | 1404 | 0.99 |
| `refstore` 1thr | 2199 | **1459** | **1.51** | 2230 | **1491** | **1.50** |
| `refstore` 8thr | 2515 | 1540 | 1.63 | 1786 | 1397 | 1.28 |

**`refnull` is the control and it does not move**; the non-null arms improve
1.5–1.9× on both collectors. That signature — null flat, non-null improved,
collector irrelevant — is what the fix predicts and is why it is quoted rather
than the absolute walls. The gap this section is about narrows from
1719/691 = 2.49× to 924/664 = 1.39×.

## 3. What this page ruled out, and what stays ruled out

Everything in the original "RULED OUT" list stands, and is worth keeping
because each entry cost a measurement:

* **The remembered-set mutex** — 27 acquisitions in a 2M-store eight-thread
  run, `memo_hit_pct=99.99`. The four-slot edge memo does its job.
* **The accessor lock** — was an enormous cost, is fixed
  (`may_be_humongous`), and `took_regions_lock=0` for every mode.
* **The region lookup** — now O(1) arithmetic over the contiguous arena.
* **The SATB pre-barrier** — `pairnull`/`pairref` never feed it a non-null old
  value, so it is not what scaled.
* **The array read** — `refread` is flat.
* **GC pauses** — `CRATONVM_DBG_G1DIAG=1` reports zero collections in these
  runs.

The one entry that needs correcting is the last line of "Anything
collector-independent": the covariance check and the two `to_string()`s were
dismissed as "not this" on the strength of ZGC being flat. That reasoning was
sound for the phenomenon being chased and wrong about their size — with the
G1-specific phenomenon gone, they were the entire remaining non-null penalty.

## 4. Reproducing

```bash
cd <repo> && javac -d /tmp/bp probes/BarrierProbe.java
for m in primstore refnull refread refself refstore pairnull pairref; do
  for gc in G1 ZGC; do for t in 1 8; do
    <cratonvm> --java-home <jdk-25> -XX:+Use${gc}GC --Xmx 1500m \
      -cp /tmp/bp BarrierProbe $m $t 4000000 4096 shared
  done; done
done
```

Three things this page learned the hard way and that survive it:

* **The fourth argument matters.** An `Object[1<<16]` is 524 328 bytes, over a
  1 MiB region's `region_size / 2` humongous threshold, so sizing the probe
  there measures the humongous path instead of the ordinary one. 4096 is safe.
* **Interleave, and use the collector as the control.** Both hosts swing 2× on
  a 30-second measurement; a G1 number compared against a ZGC number from the
  same round is immune to that, and a G1 number compared against yesterday's is
  not.
* **`CRATONVM_DBG_G1ACCESSOR=1`** adds the lock/memo census at exit and is what
  settled the rset-mutex hypothesis.

## Related

- `resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817` —
  the retired page this was split out of.
- `biginteger-modpow-montgomery-FIXED-20260817.md` — the other residue filed
  out of the same pass.
