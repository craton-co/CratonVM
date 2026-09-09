# `TestMVStoreTool`'s create phase: it is not the collector, it is the address validator — 2026-09-07

**RETIRES `docs/known-issues/h2/testmvstoretool-never-finishes-its-create-phase-on-cratonvm-20260907.md`.**
That page listed three candidate causes — *allocation rate, GC pause frequency,
or the `nio`/`ByteBuffer` natives* — and asked for a CPU profile to choose
between them. The profile exists now. **The answer is none of the three.**

| | |
|---|---|
| **Status** | **Answered and priced.** One fix landed (§4), worth a measured 2.6%. The retired page's title ("never finishes ... every collector") is WRONG: the class runs to `rc=0` in 848 s on ZGC at `-Xmx2g` (§1). The residual is named in §6 and is architectural, not an MVStore defect. |
| **Where** | Azure host 2 (`20.80.105.49`), Linux, dev tip `4d7108203`, H2 2.4.249 at `apps/h2database/h2`, release binaries, `perf 6.17.13`. |
| **Reproducer** | `probes/MvsCreate.java` — the same create phase with the entry count as `argv[0]`. 13 s instead of 30 min. |

---

## 1. The measurement the old page could not take

The class hard-codes `config.big = true`, i.e. **2,000,000** entries. On
CratonVM that is a TIMEOUT on every collector, and a timeout carries no number:
every arm of the old page's table reads `TIMEOUT 1800 s`, so nothing could be
compared to anything. `probes/MvsCreate.java` lifts the create phase out with
the count as an argument, and at `N=100000` the same shape is seconds on both
VMs.

| arm (`MvsCreate 100000`) | put | remove | total create |
|---|---:|---:|---:|
| **HotSpot 25.0.3, `-Xmx256m`** | 235 ms | 222 ms | **545 ms** |
| CratonVM G1, `-Xmx1g` | 4 659 | 7 837 | **12 989 ms** (24x) |
| CratonVM G1, `-Xmx256m` | 7 574 | 12 794 | **20 963 ms** (38x) |
| CratonVM ZGC, `-Xmx1g` (N=50000) | — | — | 8 396 ms |
| CratonVM Generational, `-Xmx1g` (N=50000) | — | — | 11 931 ms |
| CratonVM `--nojit`, `-Xmx1g` | 18 013 | 14 256 | **33 381 ms** |

Scaling at `-Xmx1g`, G1: 25 000 → 3 198 ms, 50 000 → 6 659, 100 000 → 12 989,
200 000 → 27 455. **Linear.** The old page's superlinear reading came from
running a 20x larger live set against a 256 MB heap, not from the workload.

**At `-Xmx256m` it stops being linear, and the `remove` phase is where it
goes.** 100 000 entries: put 7.6 s, remove 12.8 s. 500 000 entries: put 63.9 s,
remove **398.7 s** — five times the entries, thirty-one times the remove. The
same phase at `-Xmx1g` scales linearly to 200 000, so this is heap pressure
finding a phase to spend itself on, not an algorithmic defect in `MVMap.remove`.
Eight `MvsCreate 500000` runs at `-Xmx256m` completed (G1 ×3: 466 / 237 / 173 s;
Generational ×3: 220 / 203 / 168 s; G1 + `CRATONVM_G1_JIT_MARK_DRIVER=1` ×2:
164 / 127 s).

**ZGC did not complete any of them.** It OOMed in 9-10 s at 500 000 (3/3), and
at 100 000 and 200 000 too, with 88% of the heap free. That was a distinct
defect with its own cause matrix and its own page — and it is what the old page's
ZGC row (`OutOfMemoryError ... native reference array of length 14053`) was.

**FIXED 2026-09-08** (`docs/internal/fixed-suite-bugs/gc/zgc-oom-on-mvstore-was-returned-frame-residue-FIXED-20260908.md`):
the compactor was being refused by a returned compiled frame's leftover return
address, misread as a live unregistered JIT frame. `MvsCreate 500000` at
`-Xmx256m` on ZGC is now `rc=0` in 101 s where it was `rc=1` in 9-10 s. **The
rows in this section are pre-fix and the ZGC column wants re-taking**; the §4
address-validation finding is unaffected, since that arm never collects.

The whole class, `-Xmx256m`, for the record: **HotSpot `rc=0` in 17 s**, with
`Created in 5661 ms` — not "about four minutes", which is what the old page
quoted from a Windows run.

**And the class DOES finish — the retired page's title is wrong.** It is called
*"never finishes its CREATE phase on CratonVM — every collector"*. Run at
**`-Xmx2g`**, eight times the heap its arms used, with a 90-minute budget:

| arm, whole class, `-Xmx2g` | result |
|---|---|
| G1 | `rc=124`, TIMEOUT at 5 400 s, still in create |
| **ZGC** | **`rc=0` in 848 s** — every phase |

```text
Created in 450006 ms.      Compacted in 58972 ms.
Compacted (compressed) in 34893 ms.    Re-compacted in 41959 ms.
Re-compacted (compressed) in 64915 ms. Verified in 152594 ms.
rc=0 wall=848s tag=cvm-zgc-2g
```

So the honest statement is **"needs 2 GB and the right collector, and is then
about 50x HotSpot"** (848 s against 17 s; 450 s against 5.7 s on create alone) —
not "never finishes". Two things follow:

* **Heap DOES move it, and the collector choice moves it more.** ZGC finishes
  where G1 times out at the same heap; below ~512 MB ZGC instead OOMs in
  seconds (§ the ZGC page). The collector that is unusable at 256 MB is the one
  that passes at 2 GB.
* **It is still the mutator that sets the floor.** 450 s of create with a
  collector that is keeping up, against HotSpot's 5.7 s, is the same ~24x of §1
  compounded by the phase costs — not a pause problem.

---

## 2. The three candidates, refuted by name

### Not GC pause frequency — and not by a little

`CRATONVM_GC_STATS=1`, `MvsCreate 100000`, G1:

| heap | collections | GC time | wall |
|---|---:|---:|---:|
| CratonVM `-Xmx1g` | **`no collection has run yet`** | 0 ms | 20 759 ms |
| CratonVM `-Xmx256m` | **1** young | 141.8 ms | 18 937 ms |
| HotSpot `-Xmx256m` | 2 young | 26 ms | 545 ms |

At 1 GB the collector **never runs at all** and CratonVM is still 38x HotSpot on
that run (24x on the uninstrumented one in §1 — both rows are the same binary
on a box whose load average moves between 5 and 20 within the hour, which is
exactly why the A/B in §4 runs its arms concurrently). At 256 MB it runs
**once**, for **0.75% of the wall**. Whatever this workload is spending its time
on, the collector is not doing it.

**This refutation is scoped to the size it was measured at, and the page it
retires was measured at another one.** At the class's own `N=2000000` under
`-Xmx256m` the collector is very busy indeed: a 2 401 s run of the real class
reached `Phase 3.5 resurrection RAN (#8192)` — **8 192 pauses, ~3.9 per
second**, matching the old page's "~4.4 GC pauses per second" — and never left
the create phase. So the honest statement is a two-term one:

1. a base gap of **24x with the collector idle**, which is what this page is
   about; and
2. on top of it, at the class's own size, a live set that does not fit
   `-Xmx256m` the way HotSpot's does, so the collector runs thousands of times.

Term 2 is a FOOTPRINT question, not a pause-efficiency one, and this page does
not answer it. What it does establish is that term 1 exists on its own and is
large enough to make the class untenable at `-Xmx256m` before term 2 is even
counted.

### Not the allocation rate, as a *collector* cost

Same rows: an allocation rate that produced one young pause in 19 seconds is not
an allocation rate the collector is struggling with. (The allocator's own
per-object cost is inside the 90% of §3 and is not separable from it here.)

### Not the `nio`/`ByteBuffer` natives

No `java.nio` native appears in the profile as its own symbol, and the whole
native-dispatch path it would arrive through — `safe_native_call_impl` 1.63%,
`NativeContextImpl::get_array_element` 1.83%,
`try_jit_site_cached_native_dispatch` 1.42%, `forward_jit_reference_args` 0.95%
— sums to under 6% of self time with every other native in it. The
`ByteBuffer` contract was then tested directly rather than inferred from that:
`probes/MvsWriteBuffer.java` drives `org.h2.mvstore.WriteBuffer` through exactly
the `ObjectDataType.StringType.write` sequence (2 000 000 rounds plus 20 000
3 000-char strings), and `probes/MvsGrowBarrier.java` re-implements
`WriteBuffer.ensureCapacity`/`grow` so the field reassignment happens 4 124
times inside a JIT-hot loop. Both pass on G1, ZGC and Generational, JIT and
`--nojit`, at `-Xmx256m` and `-Xmx64m`.

### Not the conservative JIT-frame root scan either

This one is worth stating because `is_addr_in_live_region`'s own comment blamed
it, and that comment sent this investigation to the wrong place for an hour.
`CRATONVM_DBG_JIT_SCAN_PROF=1` on the same run:

```
[cratonvm] JIT scan prof: scans=0 cache_hits=0 (0.0%) band_scans=0
                          band_words=0 band_max_bytes=0 precise_frames=0
                          jit_entries=1617960
```

1.6 M transfers into compiled code and **zero** band scans. The comment has been
corrected in place.

---

## 3. What it actually is

`perf record -F 199 -g`, G1, `-Xmx256m`, `MvsCreate 100000`. Top self-time:

| % self | symbol |
|---:|---|
| 13.99 | `G1Collector::is_addr_in_live_region` |
| 9.24 | `G1Collector::is_object_address_inner` |
| 3.15 | `VmHeap::is_object_address` |
| 2.88 | `VmHeap::set_array_element` |
| 2.57 | `VmHeap::load_and_forward_inner` |
| 2.54 | `G1Collector::post_write_barrier_rset` |
| 2.37 | `G1Collector::get_array_element` |
| 2.07 | `cratonvm_types::value::record_object_ref_payload_slow` |

**~26% of the whole run is the heap-address validator.** `perf report
--sort dso` puts **82.5% of self time in the `cratonvm` binary itself**, with
`[JIT] tid N` — the compiled code that is actually executing the Java program —
at roughly 10% and libc (`memmove`, `memcmp`) at 3%. The VM spends about nine
tenths of this workload in its own runtime helpers.

The census agrees, and gives the population its size:

```
[cratonvm] g1 is_object_address: calls=495442426 accepted=495426442 total=17761ms
```

**495 million calls, 99.997% of them ACCEPT.** (`total` is inflated by the
census's own two `Instant::now` calls per invocation; `perf`'s 26% is the number
to quote.) That is not a conservative scan rejecting stack garbage. It is a
per-access validation of pointers that are already valid.

### Who calls it

`perf`'s call graphs are useless here — DWARF unwinding on this optimized build
returns self-recursive frames and LBR is unavailable on the virtualised PMU,
which is the same wall `MEMBERSHIP_WALK_BY_SITE`'s doc comment records. A
throwaway build sampling `std::backtrace::Backtrace::force_capture()` every
2 000 000th call (53 samples) attributes them:

| share | caller |
|---:|---|
| **57%** | `VmHeap::load_and_forward_inner` — the software read barrier, entered from `get_array_element`, `set_array_element`, `get_field`, `set_field`, `forward_boundary_value`, `execute_invoke_kind` |
| **17%** | `G1Collector::autobox_payload` — the reference-array unbox screen, one probe per `aaload` |
| rest | `kind_of`, `element_type_of`, `class_id_of`, `jit_checkcast`, `native_system_arraycopy`'s preamble, the JIT native-dispatch receiver |

So: **one heap-membership walk per reference field read, per reference field
write, per `aaload`, per `aastore`.** MVStore's B-tree pages are `Object[]` of
`Integer` keys and `String` values, and `pageSplitSize(1000)` makes the tree
deep, so this workload is close to a pure reference-array benchmark.

---

## 4. What landed

**`is_addr_in_live_region` was still doing a 64-bit hardware divide.**
`normalize_region_size`'s doc comment exists to explain why region size is
rounded to a power of two — *"Region `i` starts at `arena_base + i * region_size`,
so 'which region owns this address' is `(addr - arena_base) / region_size`. With
a power-of-two size that is a shift; without one it is a 64-bit hardware divide
on the hottest read in the collector"* — and F-09 converted
`lookup_region_for_addr` and `classify_candidate_header_view` accordingly. It
missed the third site, which is the hottest of them all: `is_addr_in_live_region`
is the whole body of `is_heap_addr` and the gate of `is_object_address`.
`classify_candidate_header_view`'s own comment even says *"this one is called
once per HOLDER rather than once per reference, but it is the same argument and
the two should not drift"* — they had drifted, into the third site neither
comment mentions.

Fixed, with a `debug_assert_eq!` against the divide so the three cannot drift
again.

**Measured: 2.6%.** Five concurrent paired arms (both binaries started together,
so a host-load excursion lands on both — sequential ABBA is not trustworthy on
this box), `MvsCreate 100000`, `-Xmx1g`, G1:

```
base: 20506 22947 23141 23989 30351   median=23141
fix1: 20846 22330 22544 23482 29468   median=22544
```

fix1 wins 4 pairs of 5. Real, small, and honestly reported: `div r64` on this
CPU with small operands is nothing like the "tens of cycles" the F-09 comment
assumed, and two ablations bound the rest of the function the same way —
`CRATONVM_G1_NO_LIVE_REGION_MEMO=1` measures **11 570 ms against 11 859 ms with
the memo**, i.e. the per-thread memo *costs* slightly more than the
`regions.read()` it saves on a single-threaded run, at a 98.8% hit rate
(`hit=237350947 miss=2920407`).

**Also landed: a reader for `MEMBERSHIP_WALK_BY_SITE`.** Those counters have
been incremented at six JIT-helper sites since they were added, and
`membership_walks_by_site()` — their only reader — **had no caller**. The
diagnosis was in the binary and nothing could print it. It now rides the same
gate as the `is_object_address` total.

---

## 5. What is refuted for someone else's benefit

`gc-cross-collector-common-work-20260905.md` item 5 closed *"give G1 the exact
object-start bitmap"* as **refuted**, on the grounds that the predicate is
*"0.24% of a run (289,693 calls, 12 ms of 4.9 s)"*. On THIS workload the same
predicate is **495 million calls and 26% of the run**, so the "nothing to
short-circuit" half of that refutation does not generalise. The other half still
holds and is the reason not to build the bitmap anyway: **99.997% of calls
already accept**, so an accept-only index saves the tail, not the body. The body
is the cost.

---

## 6. The residual, stated honestly

Take the entire address-validation family to zero — all 26% of it — and this
workload is still **17.6x** HotSpot at `-Xmx1g` and **28x** at `-Xmx256m`. The
H2 suite's own baseline is 5.3x
(`docs/internal/fixed-suite-bugs/h2-suite-bugs/README.md`), so a factor of three
to five is still specific to a reference-array-dense workload — and it is spread
flat
across the read barrier, the array accessors, the write barrier, the provenance
bitmap and the native-dispatch glue, with no single line item above 3% once the
validator is removed. There is no MVStore defect at the bottom of this page.
**`TestMVStoreTool` completes only at 2 GB on ZGC, 50x slower than HotSpot, and
nothing in this page's scope will change that** — the flat distribution is the
floor, and no heap size or collector reaches under it.

Two nominations that came out of the sampling and were NOT taken, with the
reason:

1. **`G1Collector::get_array_element` calls `autobox_payload` on every non-null
   `aaload`** (17% of the population above). The interpreter's ZGC fast path
   already replaced that with `autobox::header_is_wrapper` — one header compare,
   no probe — and `header_is_wrapper`'s own doc explains why the
   `wrapper_exists()` latch in front of it is permanently open in every process
   (the class-mirror populator arms it at bootstrap). Carrying that back to G1
   is worth ~4% of CPU. **Not taken here** because it trades a validated read
   for an unvalidated one on exactly the family
   `docs/internal/fixed-bugs/g1-eight-byte-write-at-a-live-objects-base-FIXED-20260908.md`
   is about — a stale reference-array element read as an object — and this lane
   had no way to bound that risk.
2. **The read barrier's entry walk.** `load_and_forward_inner` validates before
   reading a forwarding word, on every reference access, in a process where **no
   collection has ever run**. A "nothing has relocated yet" short-circuit is
   sound and free on the 1 GB arm, and worth nothing on any arm that does
   collect, which is why it is a note rather than a patch.

## 7. Corpus note, carried over

`/c/craton/h2corpus`'s `classes/`, `test-classes/` and `cp.txt` were deleted on
2026-09-06 17:46 (and `/c/craton/h2root/target/{classes,test-classes}` are
SYMLINKS into them), so every Windows-side H2 workload broke at once. This page
was measured on the Linux host's own checkout at
`/data/cratonvm/apps/h2database/h2`, H2 **2.4.249**, HotSpot `rc=0` on it. Any
comparison against a measurement taken before 2026-09-06 17:46 is cross-corpus
and must say so.
