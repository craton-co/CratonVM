# ZGC's own rewrite pass faults walking a reference array

**Status: FIXED 2026-08-15.** The collector SIGSEGV'd inside itself during
compaction's reference-slot rewrite. Distinct from the JIT-frame relocation
defect fixed the same day — this one reproduced with `--nojit`, so no compiled
frame was involved.

## The fault

```
Faulting access: read at address 0x000001DD2E9D0000     <- page-aligned
thread: "main-vm"
jit: guarded compiled frames live process-wide: no (quiescence depth=0)
jit: 0 compiled code range(s), cache generation 0
```

Symbolized against the matching binary (`CRATONVM_SYMBOLIZE`, PDB in place):

```
0x316B6F  cratonvm_gc::zgc::impl$23::reference_slots+0x1AF   gc/src/zgc.rs:7128
0x3119D1  cratonvm_gc::zgc::ZgcRealHeap::relocate_stw+0x3721 gc/src/zgc.rs:4114
0x306767  cratonvm_gc::zgc::impl$30::collect_garbage+0x6807  gc/src/zgc.rs:8582
0x389548  cratonvm_gc::vm_heap::VmHeap::collect_garbage_with_finalizers  vm_heap.rs:1509
```

`zgc.rs:4114` is the **rewrite pass** — the loop over `live_now` that re-points
every survivor's reference slots after the slide. `zgc.rs:7128` is inside
`reference_slots`' `ObjectKind::Array` arm, striding `array_length()` elements.

A **page-aligned** faulting address is the signature of walking off the end of
a mapped region, so the walk is reading past the object: either
`array_length()` is not this object's, or the base being walked is no longer
the object the walker thinks it is.

## Root cause: "this page is selected" is not "these bytes are free"

An object's page membership is decided by its **base**. An object based in an
unselected page and extending across the boundary keeps its tail inside the
selected page above, and it is not in `survivors`, so it never moves. The slide
only ever checked page membership.

The destination probe aimed straight at the hazard. When a span touched an
unselected page it restarted at `base + (blocked + 1) * PAGE` — the first byte
of the next page, which is exactly where a straddler from the page below lies.
`slide_floor` has the same shape.

So the slide memmoved a survivor over the tail of a live object. The same bytes
then belonged to two objects, and one of them was eventually read as a header.

**The workload said so in ASCII.** With the walkability guard in place, a
`--nojit` netty run reported 16 unwalkable rewrite targets, all
`registered=true`, whose headers decode as text:

| field | value | as bytes |
|---|---|---|
| `class_id` | `0x41524150` | `PARA` |
| `num_slots` | `0x444f494e` | `NOID` |
| neighbours | | `io/n`, `etty` |

`PARANOID` is netty's `ResourceLeakDetector.Level`. The registry held bases
pointing into **string data**.

## The fix, and the result

The probe now carries an **obstacle list**: extents of live objects based in
unselected pages that cross a boundary. At most one per boundary, so a sorted
`Vec` and a short scan. `slide_floor` is raised past an obstacle too.

`--nojit`, 12 interleaved reps each, same runner, one class per VM:

| arm | SIGSEGV | unwalkable targets |
|---|---|---|
| before | **2/10** | 16 in the crashing run |
| after | **0/12** | **0** |

All twelve report `found=3 started=3 ok=2 failed=1`, matching G1.

## Two defensive changes that came with it

**The array arm of the slot walker had no plausibility screen.** Its
legacy-object sibling refuses `num_slots > 1<<24` before striding; the array
arm trusted `array_length()` outright, because `array_data_size` refuses only
integer OVERFLOW. A clobbered length of a few hundred million with 4-byte
elements sizes cleanly to a couple of gigabytes and the walk strides all of it.
`alloc_size` shared the gap. Both are now bounded by
`MAX_PLAUSIBLE_ARRAY_LEN`.

Worth keeping even now the corruption is fixed: it is the difference between a
SIGSEGV inside the collector and a logged, skipped object. The test for it
reproduces the production fault **in-process** — with the screens removed the
test binary exits `0xc0000005 STATUS_ACCESS_VIOLATION` instead of failing an
assertion.

**`rewrite_target_is_walkable`** checks each survivor is sizable and fits the
arena before the rewrite walks it, and reports the ones that are not. That
guard is what produced the ASCII evidence above; without it the crash names
only the collector.

## Traps recorded

* **The obvious check in the guard is wrong.** "Is this still a registered
  base?" cannot be asked in the rewrite loop: the object-start registry is
  rebuilt AFTER it, so during the rewrite it still holds every survivor's
  PRE-slide base while `live_now` holds post-slide ones. Gating on it skips
  precisely the objects that moved.
  `collect_garbage_with_compaction_on_rewrites_roots_and_keeps_the_graph`
  caught this immediately. It is now reported and never vetoes.
* **`was_vacated=false` is not "the slide didn't do it".** `moved_from` covers
  one cycle; an address vacated nine collections ago also reports false. Every
  offender above reported false and the slide was nonetheless the cause.
* **The invariant test passed vacuously first.** Only ~4 objects cross a page
  boundary in a 10 MiB fill, and with a third of objects live the fixture
  regularly contained no LIVE straddler — and a dead one cannot be written into
  by mistake. The fixture now roots every straddler deliberately and asserts one
  exists before judging anything.

## Related

- `docs/known-issues/netty/zgc-resourceleakdetector-corpse-read-20260815.md` —
  the JIT-frame half, and the instruments (`CRATONVM_DBG_ZGC_CORPSE`,
  `CRATONVM_DBG_JIT_NAMES`, `CRATONVM_SYMBOLIZE`) used to separate the two.
- The 2026-08-14 fix for the slide crossing unselected PAGES is this defect's
  direct predecessor and the reason it was hard to see: that fix made the probe
  page-correct, which reads as "the destination probe is handled". It is the
  same probe and the same rule one level finer — pages, then bytes.
