# `IntObjectHashMapTest` SIGSEGVs before its first test — FIXED

**Status:** FIXED (2026-08-15). `io.netty.util.collection.IntObjectHashMapTest`
runs **12/12 clean, 35/35 tests, under `-XX:+UseZGC`** on the branch tip, and
28/28 on pristine `dev`. On the binary the open page was filed from it is
**8/10 bad** (4 × SIGSEGV, 4 × hang).

The page asked for four things it had not done — an `hs_err` read, a `--nojit`
arm, a collector comparison, and a narrowing of which test faults. The answer
came from none of them: **symbolising the fault pc** named the subsystem in one
step, and the collector comparison the page wanted turned out to be the whole
story.

Two defects, same family, found by taking this page seriously. One was already
fixed on `dev` and this page had gone stale without anyone noticing; the other
was live on `dev` on the day this was written, and is fixed here.

---

## Part 1 — the crash on this page: already fixed by `6b21a496e`

### What it actually was

The fault pc symbolises, on the binary the page was filed from:

```text
0x59c7b6fc8922 -> parking_lot::condvar::Condvar::notify_one_slow
                  <- cratonvm_vm::threading::monitor::Monitor::exit
```

A `Monitor` whose memory had been released while a thread still held it — not a
JIT defect at all, which is why "fault pc is in NO live registered code buffer"
was the correct and unhelpful reading of the report. `vm/src/threading/` is
**byte-identical** between the page's commit and current `dev`, so nothing about
`Monitor` changed; what changed is who was freeing its mark-word reference.

That is D3 of `6b21a496e` ("R6 audit"), in its own words:

> `MonitorCleanup::prune_dead` received PRE-SLIDE addresses. Compaction breaks
> [its exactness precondition] in the commonest way possible: survivors slide
> DOWN into space vacated by dead objects. Measured on a four-page fixture, 169
> LIVE object bases reached `prune_dead` in one collection, each a released
> mark-word reference on a live object's monitor — **a use-after-free reachable
> from any synchronized block.**

`Monitor::exit` is a synchronized block's exit. The hang the page recorded
("one run hung instead of crashing, 240 s cap, no output") is the same defect's
other face: a freed monitor whose `notify` is lost rather than faulting.

### Measured — one class, one flag varied

Azure host 2, `apps/netty-suite-runner`, one class per process, `--Xmx 1500m`.

| binary | commit | ZGC runs | SIGSEGV | hang | clean |
| --- | --- | ---: | ---: | ---: | ---: |
| the page's | `c017029b3` | 10 | 4 | 4 | 2 |
| the fix's parent | `a3e5a0405` | 8 | 4 | 0 | 4 |
| **the R6 audit** | **`6b21a496e`** | **10** | **0** | **0** | **10** |
| current `dev` | `fd46ad2fd` | 28 | 0 | 0 | 28 |
| current `dev` + `CRATONVM_DBG_GC_STRESS=4194304` | `fd46ad2fd` | 10 | 0 | 0 | 10 |
| this branch | | 12 | 0 | 0 | 12 |

Every crashing run reproduced the page's signature **bit for bit** —
`r10=0xee5fffebdfffff00`, `rbp=0x20042496501` — on a different binary, at a
different load address, three weeks later. That constant is what identifies the
runs as one bug rather than a family.

### Compaction is the trigger, and that is the second measurement

| binary | arm | runs | bad |
| --- | --- | ---: | ---: |
| `c017029b3` | `-XX:+UseZGC` (compaction default-on) | 10 | **8** |
| `c017029b3` | `-XX:+UseZGC` + `CRATONVM_ZGC_RELOCATE=0` | 5 | **0** |

So this class belongs to the cluster
`zgc-specific-sigsegv-cluster-20260814.md` describes, and the page's own
"attributed to `dev` rather than to that branch" reasoning was right.

### What this page got wrong, and it is worth naming

> The rate moved from 1-in-4 to 2-in-3 within an hour on the same binary and the
> same quiet box, so treat "it passed" as no evidence.

Correct, and the reason is not randomness: **the rate tracks box load.** The
runs here that crashed clustered exactly when a build and three other arms were
running; the two clean runs on `c017029b3` are its last two, after everything
else had finished. A "quiet box" reading and a "loaded box" reading of this
class are different experiments.

---

## Part 2 — the same family, still live on `dev`: the pin producers were G1-only

Re-running the page against current `dev` says "fixed", and stopping there would
have missed that **the mechanism behind it is still half-wired**.

`gc_quiescence::pinned_jit_roots_snapshot()` is a two-sided contract:

* the **consumer** — a moving collector dropping those pages/regions from what
  it is about to relocate, because a conservatively-scanned JIT-frame root is
  over-approximate (`is_heap_addr` is a range check, so an interior pointer or a
  plain `long` can present as a root) and therefore names a slot the collector
  **cannot rewrite**;
* the **producer** — the VM's root deposits filling the registry.

`caf25c3d1` (2026-08-14) gave ZGC the consumer. All four producers stayed gated
on `shared.mem.heap.is_g1()`:

| producer | file |
| --- | --- |
| the initiator's own JIT frames | `vm/src/memory/roots.rs` |
| a parked mutator's safepoint / native-boundary deposit | `vm/src/runtime/interpreter/gc_and_alloc.rs` |
| a blocked thread's deposit | `vm/src/vm/vm_exec.rs` |
| a forcibly-frozen peer's roots | `pin_frozen_peer_roots_for_g1` |

So under ZGC the snapshot was **empty on every cycle** and the consumer dropped
nothing. A capability that is false everywhere reads as an absence, not as a
bug — and the gc-crate test that covers it,
`a_conservative_jit_root_pins_its_page_against_relocation`, calls
`add_pinned_jit_root` **itself**, so it exercises the consumer over a snapshot
the VM would never have produced. It passes with every producer deleted.

### The witness: one object in 200 000 with a null `final` field

`probes/UnmodifiableListIteratorJitProbe` — a probe that already existed, for an
unrelated Spring Boot DevTools failure — died with a bare
`NullPointerException` on `--real-jdk`, and passed under `--nojit`. Narrowing it
(`probes/JitFrameRootRelocationProbe`, added here) gives the exact shape:

```text
iterations     = 200000
nullFields     = 1          <- delegates == null, on ONE Holder
first bad at   = 10191      <- deterministic, five runs for five
```

`Holder`'s constructor is `this.delegates = Collections.unmodifiableList(copy)`.
Its `this` lived only in a compiled `<init>` frame; ZGC's page slide moved the
object; the conservative frame slot was not — and cannot be — rewritten; and the
`putfield` landed in the vacated span. `main` kept the relocated copy, whose
field was never written.

Four independent levers, each of which alone makes it disappear, and together
they name the mechanism with no inference left:

| lever | nulls |
| --- | ---: |
| (none) | 1 |
| `CRATONVM_DBG_GC_STRESS=1048576` | **67** |
| `CRATONVM_JIT_OSR=0` | 0 |
| `CRATONVM_JIT_DENY=…$Holder.<init>` | 0 |
| `-XX:+UseG1GC` / `-XX:+UseSerialGC` | 0 |
| `CRATONVM_ZGC_RELOCATE=0` (even under GC stress) | 0 |

G1 zero is the point: G1 has the producers.

### The fix

`VmHeap::pins_conservative_jit_roots()` — one predicate both sides ask,
replacing `is_g1()` at all four producers.

* `G1` → true (it always evacuates; learned 2026-08-11).
* `Generational` → false (its young sweep runs NON-MOVING while any thread is in
  JIT, so nothing moves and a pin would be pure cost).
* `Zgc` → `relocation_requested()`. Asking *intent* and not the `vm_init` safety
  gate is deliberate: over-pinning when the safety gate later refuses costs one
  page of reclaim, under-pinning corrupts the heap. Gating on the flag is also
  what keeps `CRATONVM_ZGC_RELOCATE=0` an A/B re-run rather than a second code
  path.

`pin_frozen_peer_roots_for_g1` is renamed `pin_frozen_peer_roots_for_moving_collector`.

### The price, measured rather than assumed

Page-granular pinning on a 2 MB grid, published by every thread at every
deposit, is the obvious way to convert a memory-corruption bug into an
`OutOfMemoryError` — this arena reclaims the middle of the heap only by sliding.
`CRATONVM_DBG_ZGC_PINS=1` (added here) prints the three counts that settle it.
On `io.netty.util.ResourceLeakDetectorTest`, the most JIT- and thread-dense
class to hand:

```text
[ZGC_PINS] jit_roots=1083 selected_before=12   pages_dropped=12 selected_after=0
[ZGC_PINS] jit_roots=1076 selected_before=303  pages_dropped=27 selected_after=276
[ZGC_PINS] jit_roots=899  selected_before=748  pages_dropped=19 selected_after=729
...
[ZGC_PINS] jit_roots=23   selected_before=748  pages_dropped=2  selected_after=746
```

1 083 conservative roots withhold **27 of 748 pages — under 4%**, because they
cluster onto the pages the mutators are allocating from. The first cycle drops
all 12 of its 12 pages and the selector correctly does nothing that cycle. This
is not the cost that would force object-granular pinning.

### Regression evidence

`probes/` diffed against HotSpot JDK 25, both arms, on this branch **and on a
binary built from pristine `origin/dev`** — the control is what makes the
"IMPROVED" rows a measurement:

```text
                                       arm         fix   dev
JdkOnlyCollectionViewProbe             both          0     0   same
ViewClassProbe                         both          0     0   same
SnapshotIteratorShapeProbe             both          0     0   same
StrictIterPrimitivesProbe              both          0     0   same
MapViewBehaviourProbe                  both          0     0   same
ListOutOfBoundsProbe                   both          0     0   same
SubListBehaviourProbe                  3 arms        0     0   same (90/90)
ImmutableCollectionsDifferentialProbe  --real-jdk    0    16   IMPROVED
UnmodifiableListIteratorJitProbe       both          0     6   IMPROVED
ListItrInterfaceProbe                  both          8     8   same
```

`cargo test -p cratonvm-gc`: 1 570 + 84 pass, 0 fail.
`cargo test -p cratonvm-types`: 565 pass.
`cargo test -p cratonvm-native-collections --all-targets`: one pre-existing
failure, `treemap_for_each_reads_forwarded_action_and_pairs`
(`ClassCastException: java.lang.Integer cannot be cast to java.lang.Comparable`),
**reproduced with `native-collections/src/lib.rs` reverted to `origin/dev`** —
not this branch's.

### What it did NOT fix

`io.netty.util.ResourceLeakDetectorTest` — the open residual of
`zgc-specific-sigsegv-cluster-20260814.md` — **stops SIGSEGVing** (0 crashes in
6 runs here, against 2 in 5 on `dev`) and now returns G1's own answer under ZGC,
`3 started / 2 ok / 1 failed`. It does not PASS: its remaining failure is
`NoSuchMethodError: 'int java.lang.Byte.addAndGet(int)'`, which reproduces
identically under `-XX:+UseG1GC` on pristine `dev` and is therefore
collector-independent and a different owner's. Two of six runs also exhausted
the heap while five other arms shared the box; a solo run at the same `--Xmx`
did not.

---

## Repro

```bash
# the crash, on the binary this page was filed from
cd apps/netty-suite-runner
for i in 1 2 3 4 5; do
  cratonvm --java-home <jdk25> --Xmx 1500m @common.args -XX:+UseZGC \
    -Dcraton.batch=1 CratonRunner io.netty.util.collection.IntObjectHashMapTest \
    > /tmp/iohm-$i.out 2>&1
  echo "run $i rc=$?"        # 139 = SIGSEGV, 124 = hang, 0 = pass
done
addr2line -f -C -e <binary> $((<fault pc> - <load base>))   # -> Monitor::exit

# the pin gap, on any binary that predates this branch
javac -d /tmp/p probes/JitFrameRootRelocationProbe.java
cratonvm --real-jdk --java-home <jdk25> -cp /tmp/p JitFrameRootRelocationProbe
#   nullFields = 1   first bad at = 10191
CRATONVM_DBG_GC_STRESS=1048576 cratonvm --real-jdk … JitFrameRootRelocationProbe
#   nullFields = 67
CRATONVM_ZGC_RELOCATE=0        cratonvm --real-jdk … JitFrameRootRelocationProbe
#   nullFields = 0
```
