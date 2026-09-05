# Everything the interpreter does is ~10x, except what touches an object header

| | |
|---|---|
| **Status** | OPEN — a ranked list with one item taken and measured; the rest is unbuilt |
| **Severity** | low — no test fails on this |
| **Opened** | 2026-09-05 |
| **Sibling** | `interpreted-invoke-cost-350ns-20260825.md`, whose four passes are what made this shape visible |

The invoke page closed its third pass saying `invokestatic` and `invokespecial`
were "the two worst interpreted call shapes by a wide margin" and named them as
the next target. They were taken, and the result is that **no call shape is an
outlier any more**. Re-measuring the whole operation table on current dev turns
up a different partition, and it is a much cleaner one.

## The measurement

Windows 11, 24C/32T, host load ~43%, JDK 25.0.3, `cratonvm --nojit` against
`java -Xint`, min-of-5, arms interleaved in both directions inside one process,
each probe carrying its own control arm. `probes/ElemShape.java` is new; its
control has the same bytecode **count** as its array arms and touches only
locals, so the difference is the element access and nothing else.

| operation | CratonVM | HotSpot `-Xint` | ratio |
|---|---:|---:|---:|
| one bytecode, straight-line | 7.1 ns | 0.69 | 10.2x |
| tight loop iteration | 53.8 | 5.50 | 9.8x |
| `invokestatic`, 0 args | 132.6 | 14.5 | 9.1x |
| `invokestatic`, 4 args | 181.5 | 21.2 | 8.6x |
| `invokevirtual` | 183.2 | 17.1 | 10.7x |
| fixed target (private / special) | 171.9 | 15.7 | 11.0x |
| `invokeinterface`, inherited | 183.0 | 18.4 | 9.9x |
| each extra `int` argument | 12.2 | 1.67 | 7.3x |
| **`getfield` + `putfield`, own** | **56.0** | **1.10** | **51x** |
| **`getfield` + `putfield`, inherited** | **53.2** | **1.37** | **39x** |
| **`getstatic` + `putstatic`** | **86.4** | **3.45** | **25x** |
| **`iaload`** | **39.9** | **1.68** | **24x** |
| **`iastore`** | **33.9** | **1.09** | **31x** |
| **`arraylength`** | **24.4** | **0.39** | **62x** |
| **`aaload`** (over `iaload`+`ifne`) | **+47.1** | **−0.8** | — |

Two populations, and nothing in between. Everything that crosses a method
boundary, branches, or does arithmetic is inside one 7–11x band. Everything that
**dereferences an object header** is 24–62x.

`arraylength` is the one to read first: no resolution, no cache, no barrier, no
allocation — it reads one word out of a header and pushes an int, and it costs
three and a half straight-line bytecodes' worth of time to do it.

`aaload` is the worst and is not in a fast arm at all: reference arrays keep the
full barrier-aware `get_array_element`, so an `aaload` costs 47 ns more than an
`iaload` + `ifne` doing the same work, where HotSpot has the two identical to
within noise.

## The item that was taken: the registry probe

`field_ptr_for` and `prim_elem_ptr` both opened with
`ZgcRealHeap::is_object_address`, i.e. `ZObjectStartBits::contains`: one
`Acquire` load of a word of the object-start bitmap, indexed by the receiver's
own address. (One load on the hit path, not two — the `overflow_len` read beside
it is only reached when the bitmap says no. An earlier draft of this page said
two and was wrong.)

**The handler those arms replace does not make that test.**
`ZgcRealHeap::get_field` is `let header = self.header(obj)` — a bare pointer cast
— followed by `check_field_index` against that header; `set_field` is the same.
What establishes that the site's recorded offset is legal for *this* receiver is
the triple compare immediately after the probe — `class_id`, `num_slots`, compact
flag — and for arrays the `kind` / `element_type` / `array_length` tests. Those
run unchanged, and a mismatch still falls back to the full handler.

The one thing the probe bought incidentally was a stale-receiver screen: a
relocated address is pruned from the registry, so it missed and fell back to
`op_getfield`, whose `load_and_forward` heals the receiver. That heal exists for
a window these arms do not have — `op_getfield` pops the receiver into a bare
Rust local and *then* calls `resolve_field_ref`, which can load a class,
allocate and provoke a collection while the local is invisible to the root scan
(GCBARRIER-CDLWAIT-FIX, 2026-07-17). Between `peek_compact` and the header read
the quickened arm allocates nothing, takes no lock and calls nothing that can
reach a safepoint, so its window is zero-length.

Kill switch: `CRATONVM_JIT_NO_FIELD_ADDR_ELIDE=1` (`CRATONVM_JIT=-field-addr-elide`).

### Engagement first, and this one needed a trick

The switch changes a branch and nothing observable, so neither census nor clock
could say whether it reached the code — and a null result from an unengaged
switch is the failure mode this page's sibling documents twice.

`CRATONVM_ZGC_STARTBITS=0` swaps the object-start registry from the bitmap to a
`Mutex<FxHashSet>`, which makes every `contains` take a lock. That turns
engagement into a 2x2 with a prediction in each cell. `probes/FieldBurn.java`,
N = 30 M (60 M instance field accesses), wall ms:

| | bitmap registry | mutex registry | Δ |
|---|---:|---:|---:|
| elide (default) | 3617 | 3700 | +83 |
| probe restored | 4023 | 4460 | **+437** |

**The registry implementation only matters in the arm that still probes it.**
That is engagement, and it is not inferable from any counter in the tree.

### The A/B

`probes/FieldBurn.java` at N = 30 M, ten interleaved passes, alternating order,
default bitmap registry, host load ~34%. `ctl` is the same loop over locals —
the arm the switch cannot reach.

| pass | elide field | probe field | elide ctl | probe ctl |
|---|---:|---:|---:|---:|
| 1 | 3686 | 4026 | 2281 | 5203 |
| 2 | 3935 | 7957 | 2106 | 2129 |
| 3 | 3584 | 4586 | 2211 | 2217 |
| 4 | 3544 | 4072 | 2005 | 2489 |
| 5 | 3179 | 3099 | 1657 | 1698 |
| 6 | 3155 | 2937 | 1836 | 1827 |
| 7 | 2911 | 3495 | 1920 | 1886 |
| 8 | 3055 | 3313 | 1883 | 1880 |
| 9 | 3074 | 3426 | 1862 | 1856 |
| 10 | 2833 | 3018 | 1702 | 1689 |

* **field: 8/10 pairwise** in favour of the elide.
* **ctl: 5/10** — the coin flip an honest control has to give.
* min-of-10: field 2833 against 2937 (**104 ms** over 60 M accesses); ctl 1657
  against 1689 (32 ms, the noise floor). Net ≈ **1–2 ns per instance field
  access**, and the quiet tail (passes 5–10, where `ctl` is stable) puts it
  nearer 4 ns with two passes going the other way.

**This is a weak positive, and it is recorded as one.** It is not the
4/4-no-overlap standard the sibling page holds its own claims to. The change is
kept on the grounds that earned the sibling page's unmeasured deletions: it
*removes* work, restores parity with the handler it stands in for, adds no cache
and no invalidation, and its engagement is proven independently of the clock.

### The hypothesis this refutes

The elide was proposed on the theory that the bitmap is sized by the heap
(~16 MB for 1 GiB) so the probe is a random access that misses once the working
set is real. **That is wrong, and the reason is worth keeping.**

`probes/AddrProbe.java` walks receivers scattered across live sets of 4 K, 256 K
and 2 M objects. CratonVM's `getfield` delta stays flat at ~50–90 ns across all
three, while HotSpot's climbs 0 → 98 → 111 ns. HotSpot feels the header miss;
CratonVM does not — **its own per-bytecode cost is large enough to hide a cache
miss behind it**. The bitmap word and the header are also both derived from the
same pointer, so the two loads issue in parallel and the second miss largely
overlaps the first.

So the elide is worth one load, everywhere, and never more. A corollary worth
carrying to the rest of this page's list: *no* interpreter change here should be
justified by a cache-locality argument until the per-bytecode floor comes down.

## Two corrections to the tree

* **`field_fast.rs`'s module doc is stale.** It opens by explaining that
  "essentially every object the interpreter allocates is legacy" because
  `ZgcRealHeap::try_alloc_object` never set the compact shape.
  `compact_tlab_alloc_enabled` has been **default-ON since 2026-09-03**. The two
  body arms are still needed; the reasoning printed above them no longer
  describes the default configuration, and it is the reasoning a reader uses to
  decide what to touch.
* **Object body shape does not move field cost.** `probes/FieldShape.java` at
  250 k x 4, three interleaved passes, one binary,
  `CRATONVM_COMPACT_TLAB_ALLOC=0` against the default: own-field pair 122 / 120 /
  117 ns compact against 124 / 126 / 133 legacy, with the control arm moving as
  much as the difference. No separation. The compact TLAB shape bought memory —
  87.5 MB on `TestCache`, as its own note claims — and it did not buy field
  throughput.

## What is left, ranked by evidence

1. **`aaload` has no fast arm at all** (+47 ns over the equivalent `iaload`,
   against HotSpot's −0.8). Reference arrays keep the barrier-aware path. This is
   the largest item on the page and nothing has been built for it.
2. **Seven or eight per-access gate loads to read one field.** Counted through
   `getfield_fast_keyed`: `any_field_watchpoint_active`, `any_class_redefined`,
   `class_definition_epoch` and `resolution_epoch` (the last two inside
   `SiteCache::get`, on every hit), `site_stats::on()`, and
   `vacated_frames_enabled` inside `check_vacated_compact` on the push. The fix is
   the pattern this file already uses a dozen times — hoist them into the
   per-`execute_frame` word beside `fast_field_zgc`, with the same
   "observed at frame entry" contract.
3. **`arraylength` takes the `Value` round trip.** The `0xbe` arm pops into the
   16-byte `Value` enum, matches, and on the non-object path pushes it back —
   which keeps it live across the arm — then goes through the `VmHeap` enum
   `dispatch!`. For one header word. `peek_compact().as_object_ptr()` + a direct
   `header.shape` read + `push_int_unchecked` is the whole opcode.
4. **`getstatic` / `putstatic` at 25x** were not investigated. They do not go
   through `field_ptr_for` and were the internal control for this pass.

## Reproduction

```bash
javac -d /tmp/probe probes/FieldBurn.java probes/FieldShape.java \
    probes/ElemShape.java probes/AddrProbe.java

# engagement — the registry implementation must matter only when the probe runs
for bits in 1 0; do for arm in "" "CRATONVM_JIT_NO_FIELD_ADDR_ELIDE=1"; do
  env CRATONVM_ZGC_STARTBITS=$bits $arm cratonvm --java-home <JDK 25> \
      --nojit -c /tmp/probe FieldBurn field 30000000
done; done

# the A/B — alternate the arm order per pass, and print `ctl` beside `field`
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe FieldBurn field 30000000
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe FieldBurn ctl   30000000
```

`ctl` is not optional. Two of this page's four measurement attempts were
discarded because it moved as much as the arm under test.
