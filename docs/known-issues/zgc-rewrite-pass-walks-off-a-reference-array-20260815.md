# ZGC's own rewrite pass faults walking a reference array

**Status: STILL OPEN — reopened 2026-08-15, later the same day.** The
straddler fix below is real and landed; the crash it was closed against is
not gone. `io.netty.util.ResourceLeakDetectorTest` under `-XX:+UseZGC --nojit`
still SIGSEGVs, on a binary built from **pristine `origin/dev`**, and the
walkability guard still reports registered bases whose headers decode as text.
See "Reopened" at the foot of the page for the numbers and for the one
hypothesis that has since been tested and eliminated.

The "0/12 after" reading below is a **sampling artefact**, and this page's own
Measurement-traps section predicted it: it says to use "completion rate over
15+ reps" and then closed the case on 12. A ~1-in-10 event and zero in twelve
draws are entirely compatible.

**Status when written: FIXED 2026-08-15.** The collector SIGSEGV'd inside itself
during compaction's reference-slot rewrite. Distinct from the JIT-frame
relocation defect fixed the same day — this one reproduced with `--nojit`, so no
compiled frame was involved.

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

All twelve report `found=3 started=3 ok=2 failed=1`, matching G1. With the JIT
back ON — where the quiescence fix also applies — 8/8 clean, 0 unwalkable,
same counts. The residual `failed=1` is the GC-independent defect that
reproduces on every collector.

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

- the retired `zgc-resourceleakdetector-corpse-read` write-up — the JIT-frame
  half, and the instruments (`CRATONVM_DBG_ZGC_CORPSE`,
  `CRATONVM_DBG_JIT_NAMES`, `CRATONVM_SYMBOLIZE`) used to separate the two.
- The 2026-08-14 fix for the slide crossing unselected PAGES is this defect's
  direct predecessor and the reason it was hard to see: that fix made the probe
  page-correct, which reads as "the destination probe is handled". It is the
  same probe and the same rule one level finer — pages, then bytes.

---

# Reopened (2026-08-15, later the same day)

## It still crashes, on pristine `origin/dev`

Found while re-verifying this page and its companion before retiring both.
`ResourceLeakDetectorTest`, one class per VM, ZGC:

| binary | arm | reps | SIGSEGV |
|---|---|---|---|
| `origin/dev` unmodified | ZGC `--nojit` | 23 | **3** |
| this branch (cursor check) | ZGC `--nojit` | 14 | **1** |
| `origin/dev` unmodified | ZGC, JIT on | 5 | 0 |
| `origin/dev` unmodified | G1 | 5 | 0 |

Interleaved, one class per VM, no diagnostic env flags. **Do not read 3/23
against 1/14 as a rate reduction** — at these counts the two are
indistinguishable, and the guarded run that crashed did so with the cursor
check *not firing*, which is the finding that matters: there is at least one
further path to this SIGSEGV that the amplifier below does not explain.

The JIT-on arm is clean because the companion fix declines relocation while a
compiled frame is live, and on this workload that is 64 of 68 cycles — so the
JIT arm barely slides at all. `--nojit` slides every cycle, and is therefore
the arm that exercises this defect. **The two fixes are not independent: the
JIT one masks this one.**

## The evidence is the same as the original, one level less specific

The walkability guard's per-object lines, from a crashing `--nojit` run on
pristine dev — a **contiguous run of twelve registered bases**, 32 to 1176
bytes apart, every one `registered=true sizable=false was_vacated=false`, and
every `class_id` / `num_slots` decoding as printable text:

| field pair | as bytes |
|---|---|
| `796091762` / `1768710518` | `res/` `vali` |
| `1702129257` / `1818324594` | `inte` `rnal` |
| `1886680168` / `1630482234` | `http` `://a` |
| `778531439` / `1667330145` | `org.` `apac` |
| `1836592999` / `1919954796` | `g/xm` `l/pr` |

`http://apache.org/xml/properties/internal/...` — JAXP constant-pool strings.
The original found `PARANOID` and `io/netty`; this is the same failure with a
different tenant. **A run of registry entries names memory that now holds
String character data**, so either those addresses were handed back to the
allocator while the registry still listed them, or a String was written over
live objects.

One crashing run reported **492 of 27858** survivors unwalkable. Another
crashed with **zero** — so an unwalkable rewrite target is one route to the
SIGSEGV and not the only one.

## The amplifier, found and disarmed

**One unsizable header becomes hundreds of stranded live objects, in one line
of code.** The survivor loop refuses to slide past an object it cannot size —
correctly, because it cannot know where that object ends:

```rust
let Some(size) = Self::alloc_size(self.header_ref(from as *mut u8)) else {
    tracing::warn!(addr = from, "zgc relocate: unsizable survivor stops the slide");
    dest = from;
    break;
};
```

`dest` is the compaction cursor. `break` abandons **every selected-page
survivor above `from`** — all of them alive, none of them moved — and then
`compact_low_to(dest)` zeroes from `dest` upward and hands the span back to the
bump allocator. The object-start registry still names every one of them. The
next allocations write over the lot, and a cycle later the rewrite pass meets a
contiguous run of registered bases holding String data.

The crashing run on pristine `dev` says exactly this, in order:

```
line  113  WARN  zgc relocate: unsizable survivor stops the slide  addr=2200137083584
line  130  ERROR zgc relocate: 56 of 27039 survivor(s) could not be walked
```

and the **first** of the unwalkable per-object lines is `base=2200137083584` —
the same address — followed by eleven more marching upward 32 to 1176 bytes at
a time. It is a cascade, not an event: each cycle's stranded run supplies the
next cycle's unsizable headers, which strand a larger run. 56 in one crashing
run, 492 in another.

`relocate_stw` now checks the cursor against the whole live set before handing
it to the allocator: one pass over `live`, resolved through the slide's own
`from -> to` pairs, raising the cursor rather than reclaiming past anything
still live, and reporting when it has to. `dest` and `highest_pinned_end` are
derived from two *different subsets* of the live set, and every argument that
their maximum covers everything is an argument about the partition; this is a
check on the answer. It has been observed firing on this workload, in a run
that then completed.

**This disarms the cascade. It does not close the page**, for two separate
reasons, and both are worth stating plainly against the temptation to call it
fixed:

* it does not explain the **first** unsizable header — one object, where the
  crash needs hundreds — and that origin is still unknown;
* one guarded run crashed anyway, **with the cursor check not firing**. So the
  cascade is one route to this SIGSEGV and demonstrably not the only one.

## What the next investigator should do first

* **A pre-slide census is now wired in** behind `CRATONVM_DBG_ZGC_CORPSE=1`: it
  reports whether the live set was ALREADY unwalkable on entry to the slide.
  That single number splits the search space in half — "this slide broke them"
  versus "they arrived broken" — and no measurement so far distinguishes the
  two. Run it first.
* **Suspect the free list, not only the slide.** A String written over a run of
  live objects is what an allocator hands out, not what a memmove does; a
  memmove writes one object's worth. `Arena`'s low free list and its
  coalescing are the obvious place for two adjacent freed blocks to merge
  across a live object between them. `compact_low_to` drops the low free list
  wholesale, which is a hint that this boundary has been trouble before.
* **Do not measure the rate with a diagnostic flag on.** The companion page
  measured `CRATONVM_DBG_ROOT_SOURCE=1` moving the crash rate from 6/10 to
  2/10. `CRATONVM_DBG_ZGC_CORPSE=1` allocates a ledger entry per relocated
  object and has never been checked for the same effect.
* **Fifteen reps minimum, interleaved.** This page's own trap list says so and
  this page's own conclusion ignored it.

## Related

- `zgc-resourceleakdetector-corpse-read-20260815.md` — retired; its closing
  section records why the JIT-on arm no longer reaches this defect.
