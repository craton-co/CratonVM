# ZGC's own rewrite pass faults walking a reference array

**Status: STILL OPEN — reopened 2026-08-15, later the same day.** The
straddler fix below is real and landed; the crash it was closed against is
not gone. `io.netty.util.ResourceLeakDetectorTest` under `-XX:+UseZGC --nojit`
still SIGSEGVs, on a binary built from **pristine `origin/dev`**, and the
walkability guard still reports registered bases whose headers decode as text.
See "Reopened" at the foot of the page for the numbers, and "Second pass"
for what the corruption actually is -- an object-start registry that acquires
an entry INTERIOR to a live object -- together with the five hypotheses
eliminated by measurement and the instruments that did it.

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

## Second pass, 2026-08-16: the corruption has a name

**The failure is an OVERLAPPING OBJECT-START REGISTRY.** The registry acquires
an entry at an address that is *interior to a live object*, and everything
downstream sizes objects from headers: the sweep zeroes and free-lists
`alloc_size(header)` bytes from a dead base, the slide memmoves that many, and
`is_object_address` decides containment with it. One interior entry therefore
destroys its neighbours, and the wreckage is the "registered base whose header
decodes as text" this page opened with.

A new gated instrument names it directly — `zgc extent census`, run over the
sorted registry before the sweep and again at the end of the slide:

```
zgc extent census: a registered object's computed extent runs INTO the next registered object
  site="pre-sweep" base=2200137058232 size=8208 next_base=2200137058312 overrun=8128
  kind=Object class_id=1113795904 num_slots=512
  w0="0x0000020042632d40" w1="0x0200000000000000" w3="0x0000020042634808"
```

**`w0` is not a header. It is an arena pointer.** This heap's addresses are
`0x0000_0200_4xxx_xxxx`, so read as a header its low half becomes
`class_id=1113795904` and its high half `num_slots=512` — which is where the
absurd 8208-byte extent comes from. `w3` is another pointer. The registry entry
sits on a **reference slot inside a live object**, not on an allocation.

## Five hypotheses, all eliminated by measurement

| hypothesis | verdict | the number that settled it |
|---|---|---|
| the live set arrives at the slide already broken | **no** | pre-slide census `0` on every cycle of every run |
| compaction is not involved | **no**, it is required | `CRATONVM_ZGC_RELOCATE=0`: 0 overlaps, 0 crashes, every arm |
| the slide creates the overlap | **no** | post-slide census clean on the cycle before the first overlap appears |
| a retained TLAB chunk is re-issued under the retracted cursor | **no** | `tlab_retire_skipped=0` — no chunk is ever left un-retired |
| an allocation's header disagrees with the bytes reserved for it | **no** | `zgc alloc audit` never fires, including in runs that go on to overlap |

The third row is the load-bearing one and it is worth reading twice: on the run
that first shows an overlap, the **post-slide** survey at the end of the
preceding cycle is clean and the **pre-sweep** survey of the next cycle is
dirty. The entry appears while **mutators are running**, on a heap that has
compacted at least once. That is a much smaller window than "somewhere in the
collector".

## Third pass, 2026-08-16: nothing inserts it — it is overwritten in place

The second pass left "who inserts an interior address into the registry" as the
next step. **Nothing does.** Three checks were added at the two mutator-side
insert paths (`alloc_raw` and the TLAB batch), and all three read zero on runs
that go on to produce five overlaps and a SIGSEGV:

| check | asks | result |
|---|---|---|
| `double-issue` | is this address already a registered base? | **0** |
| `interior-insert` | is this address inside a registered object? | **0** |
| `stale-entry-swallowed` | is a registered base inside the span I am about to occupy? | **0** |

The first of those is the one that matters most: the arena never hands out
memory the registry still believes is live. So the allocator is exonerated, and
so is the idea that a stale entry survives a sweep and gets swallowed.

## What actually happens, in one line

The registry is snapshotted at the end of every slide — base and computed size,
taken after the rebuild and before any mutator resumes — and the next cycle's
survey reports what changed:

```
base=2200137181440  size=8208  next_base=base+96
seen_at_slide_exit=true   size_at_slide_exit=96
w0="0x0000020042651248"
```

**It was a correctly-sized 96-byte object when the slide handed the heap back,
and its header word now holds an arena pointer.** Same shape at 80 and at 40
bytes in other runs. Nobody registered anything; a *write landed on a live
object's header* while mutators were running.

That also explains the recurring `num_slots=512`: this heap's addresses are
`0x0000_0200_4xxx_xxxx`, and the header packs `class_id` in the low 32 bits and
`num_slots` in the high 32, so **every** arena pointer read as a header yields
`num_slots = 0x200 = 512`. It was never a 512-field class.

## Which narrows the writer to one shape

A well-formed store into object `O` at index `i` writes at `O + 16 + 16i`, so
it can only land on *another* object's offset 0 if the writer's base sits
exactly 16 bytes below a registered base. In one capture the words just below
the victim decode as a plausible header — `class_id=1202 num_slots=11` at
`base - 16` — for an object the registry does **not** contain.

So the writer is holding a base the registry disagrees with, by one header. The
next instrument follows directly: **check the target base in the field/array
store path** — under the same corpse gate, refuse-and-report a store whose
receiver is not a registered base, and log the receiver's class. That catches
the write where it happens and names the class doing it, instead of inferring
it from what the header looks like a collection later.

Worth knowing before starting: the slide verifier
(`CRATONVM_DBG_ZGC_VERIFY_SLIDE=1`) reports ~36 "reference slot does not
resolve to a live base" lines per run, and **every one is
`missed_rewrite=false`** — they are slots a class declares as references
holding non-reference words (the same population the `W7-84` autoboxing warning
counts), not missed remaps. Doc A's `missed_rewrites=0` stands; do not spend a
session on those lines.

## Instruments added this pass

Behind `CRATONVM_DBG_ZGC_CORPSE=1`, all O(1) or one pass, a branch when off:

* `zgc registry insert` — the three checks above, at `alloc_raw` and
  `tlab_batch`. Reports and never vetoes: refusing an insert would drop a live
  object's base and turn a bookkeeping bug into a use-after-free;
* `slide_exit_sizes` — base → computed size as the last slide left it, so the
  extent census can say `seen_at_slide_exit` / `size_at_slide_exit` and split
  "changed under us" from "arrived wrong". That field is what turned this pass.

The slide's own `registry.insert(*to)` is deliberately **not** audited: the
registry is mid-rebuild there and still holds every survivor's pre-slide base,
so every probe would report a conflict against an address the next iterations
remove. The post-slide extent census covers that site instead.

## Instruments now in the tree

All three are behind `CRATONVM_DBG_ZGC_CORPSE=1` and cost a branch when off:

* `zgc extent census` — the registry survey, at `pre-sweep` and `post-slide`,
  with the raw words at the offending base;
* `zgc alloc audit` — reserved bytes vs the header's own size, per allocation;
* the pre-slide walkability census, from the first pass.

And one counter is exported unconditionally in the shutdown summary:
`tlab_retire_skipped`, which is what ruled the TLAB story out and is how
someone re-opens it.

## Related

- `zgc-resourceleakdetector-corpse-read-20260815.md` — retired; its closing
  section records why the JIT-on arm no longer reaches this defect.
