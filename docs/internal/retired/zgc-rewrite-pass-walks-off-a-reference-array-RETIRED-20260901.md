# ZGC's own rewrite pass faults walking a reference array

**Status: CLOSED 2026-08-19, RETIRED 2026-09-01.** The bisect below names the
writer. Retirement waited on the one row the elimination table still had open --
*a raw pointer TRANSLATED out of the arena* -- which the fifth pass said needed
**a fix rather than a probe**. That fix, and the three defects reading the path
turned up, are in "Eighth pass" at the foot of the page. Everything above it is
the record as written, unedited apart from this banner.

**CLOSED 2026-08-19.** Bisected to **`aa4bc7922`** — the reference
processor wrote through a **pre-GC address** guarded only by `num_fields >= 2`, so
a `String` passed the test and a queue-head pointer landed on whatever the slide
had moved into that address. Parent `2fac8c241` crashes 3/38, `aa4bc7922` is clean
0/38, current `dev` 0/52. See "CLOSED" at the foot of the page for why every audit
on this page was blind to it: they all watch **mutators**, and the writer was the
**collector**.

**Reopened 2026-08-15, later the same day.** The
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

## Fourth pass, 2026-08-16: it is not a Java store, and not a raw native copy

The third pass ended with "instrument the store path: report a field/array
store whose receiver is not a registered base". Done, and it never fires.

`ZgcRealHeap::audit_access_receiver` asks one question on every `set_field` and
`set_array_element` — is this receiver a registered object base? — and reports
the receiver's class, whether any registered object CONTAINS the address, how
far into it, and that container's class. Across runs including two that
produced overlaps (5 and 2) and one that SIGSEGV'd:

```
s-rep2 rc=1   access_audit=0 overlaps=5
s-rep4 rc=1   access_audit=0 overlaps=2
s-rep6 rc=139 access_audit=0 overlaps=0
```

**Every Java-level field and array access has a properly registered receiver.**
`set_field_volatile` delegates to `set_field`, so it is covered by the same
check; and `Unsafe.putObject` routes through `ctx.set_field` /
`ctx.set_array_element`, so it is too. That closes the whole managed store path.

The other way an 8-byte arena pointer can land on a header is a raw native
copy, and this VM already has a detector for it: `CRATONVM_DBG_HEAPCOPY` on
`VmExec::copy_to_native_memory` prints the Java stack of any raw write whose
destination aliases the managed heap. It reads **zero** in a run that produced
an overlap.

## Which leaves one door, and it is the one the detector cannot see through

`copy_to_native_memory` opens with:

```rust
if unsafe_arena_addr_is_tagged(addr) {
    return unsafe_arena_copy_in(addr, data);
}
...
if heapcopy_dbg() && self.shared.mem.heap.is_heap_addr(addr as usize).is_some() { ... }
```

**The tagged-arena-handle path returns before the diagnostic runs.** So a write
through a tagged handle is invisible to the very detector written to catch raw
writes into the heap — and tagged arena handles are exactly the values this VM
is known to leak into native code (`0x4000_0010_…` appearing inside a `.so` is
a tagged handle, not a pointer). That is the next place to look, and the first
edit is to move the `heapcopy_dbg()` check ABOVE the tagged early return so the
two paths are instrumented alike.

## The elimination table, cumulative

| candidate writer | verdict | the number |
|---|---|---|
| a bad registry insert (three shapes) | **no** | `double-issue` / `interior-insert` / `stale-entry-swallowed` all 0 |
| the slide itself | **no** | post-slide survey clean on the cycle before the first overlap |
| the live set arriving broken | **no** | pre-slide census 0, every cycle |
| a retained TLAB chunk | **no** | `tlab_retire_skipped=0` |
| an allocation sized wrong | **no** | `zgc alloc audit` never fires |
| a Java field/array store | **no** | `zgc access audit` never fires, incl. runs that overlap |
| a raw native copy into the heap | **no** | `CRATONVM_DBG_HEAPCOPY` 0 in an overlapping run |
| a write through a TAGGED arena handle | **untested** | the detector returns before it |

Compaction remains necessary — `CRATONVM_ZGC_RELOCATE=0` has never produced an
overlap or a crash on any arm.

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

# Fifth pass, 2026-08-18: the prescribed next step cannot fire, and the table has a gap

## The tagged-handle door is closed by construction, not by measurement

The fourth pass ended by naming the tagged-arena-handle path as "the one door
left" and prescribing a first edit: move the `heapcopy_dbg()` check above the
tagged early return in `copy_to_native_memory`. **The early return is real —
that part is confirmed — but the edit would produce a probe that cannot fire,
and the door it guards cannot reach the Java heap at all.**

Two facts from the source, both cheap to re-check:

* **The probe would test the wrong value.** After the move, `heapcopy_dbg()`
  would ask `is_heap_addr(addr)` where `addr` is a *tagged handle* —
  `0x4000_0010_…`, bit 62 set. That is never a managed-heap address, so the
  condition is false on every tagged call by construction.
* **The write cannot reach the heap even so.** `ArenaStore::copy_in` resolves
  the handle to `arena.bytes[offset..end]` — a Rust-owned `Vec<u8>` in a
  `BTreeMap<i64, Arena>`, wholly separate from the managed heap — and refuses
  when `end > arena.bytes.len()`. It is bounds-checked into its own block.

So the last row of the elimination table resolves to **no**, on the same footing
as the others.

## What the table never enumerated: the handle TRANSLATED, and the bound dropped

The hazard is the mirror image of the one that was being chased. It is not a
write *through* a handle — those are bounds-checked into a `Vec`. It is
`unsafe_arena_real_ptr`, which the module's own doc calls "the one place the
arena's backing store is exposed rather than copied". It has **two production
callers, both in `vm/src/native/jni.rs`**:

* `jni_long_arg_bits` — a tagged handle arriving as a JNI `jlong` argument. Its
  doc names the case: netty-tcnative's `SSL.bioWrite(long bio, long address, int
  len)`. **The repro for this page is a netty test.**
* `direct_buffer_native_address` — `GetDirectBufferAddress`.

**Both discard the bound.** `Some((ptr, _remaining))` and
`.map(|(ptr, _len)| ptr)`. Native code receives a bare `*mut u8` into a
`Vec<u8>` with no length, and nothing checks what it writes.

Two ways that becomes a write into unrelated memory, neither instrumented:

* **past the end of the block** — nothing bounds the callee, and the length it
  does use comes from Java;
* **after the block has moved or died** — `reallocate` is
  `bytes.resize(new_size, 0)`, and `Vec::resize` may **move** the buffer; `free`
  drops it. Either way a pointer already handed out then names memory the
  process allocator has taken back.

That is a raw write of pointer-shaped bytes, from outside this process's Rust
code, invisible to every audit this page has added — which is exactly the writer
it has been looking for: one that puts an 8-byte arena pointer on a live
object's header and leaves no trace in `zgc access audit`, `zgc alloc audit`,
`zgc registry insert` or `CRATONVM_DBG_HEAPCOPY`.

**Stated as a candidate, not a conclusion.** It has not been run against the
repro — see below.

## The instrument

`unsafe_arena_translation_stats() -> (translations, stale_on_realloc,
stale_on_free)`, always on (a `BTreeMap` insert per translation, and translations
are rare):

* every `real_ptr` records the block it exposed;
* `reallocate` and `free` report — with the handle and the translation count —
  when they touch a block whose pointer is outstanding.

**Nonzero `stale_on_*` means the mechanism is live on the workload.** Zero does
*not* clear the path, because the unbounded-write half leaves no trace here; that
half needs the bound to be carried to the callee rather than dropped, which is a
fix rather than a probe.

## The elimination table, corrected

| candidate writer | verdict | the number |
|---|---|---|
| a bad registry insert (three shapes) | **no** | all 0 |
| the slide itself | **no** | post-slide survey clean the cycle before |
| the live set arriving broken | **no** | pre-slide census 0 |
| a retained TLAB chunk | **no** | `tlab_retire_skipped=0` |
| an allocation sized wrong | **no** | `zgc alloc audit` never fires |
| a Java field/array store | **no** | `zgc access audit` never fires |
| a raw native copy into the heap | **no** | `CRATONVM_DBG_HEAPCOPY` 0 |
| ~~a write through a TAGGED arena handle~~ | **no** | bounds-checked into its own `Vec`; and the prescribed probe tests a handle against `is_heap_addr` and cannot fire |
| **a raw pointer TRANSLATED out of the arena** | **untested** | `unsafe_arena_translation_stats`, added here |

## Not run against the repro, and why

`ResourceLeakDetectorTest` under `-XX:+UseZGC --nojit` at ~3/23 needs the netty
suite and enough reps to see a 1-in-8 event. The Azure host was at **load 38 with
31 GB of 31 GB used and OOM-killing builds** for the whole session. Recorded so
the next person starts from "run the instrument" rather than from "read the
arena".

# Sixth pass, 2026-08-18: two more writers closed by construction, and the evidence re-read

## The two remaining bulk writers into the heap are both bounded

Every audit on this page hooks `set_field` / `set_array_element`. The obvious
gap is a **bulk** writer that touches heap memory without going through either.
There are exactly two, and both are closed by reading them:

* **`System.arraycopy`.** `native_system_arraycopy` copies **per element through
  `ctx.set_array_element`**, so it is inside the audited path, not outside it.
  (Worth stating because the card-barrier work on the same collector concluded
  the opposite about *call-site* coverage — `arraycopy` defeats a per-call-site
  barrier precisely because it is one call doing N stores. It does not defeat a
  per-*accessor* audit, which is what `audit_access_receiver` is.)
* **`Unsafe.copyMemory`, off-heap → heap.** Routes to
  `unsafe_array_write_bytes`, which **refuses reference arrays outright**
  (`et == Reference → false`), then bounds-checks `start + bytes.len() > total`,
  and whose byte/boolean fast path calls `write_byte_array_from` — verified to
  re-check kind, element type and `dst_off + src.len() > len` and to write
  nothing on mismatch. It cannot write past an array and cannot write a
  reference.

Neither can put an 8-byte heap pointer on a header.

## Re-reading the evidence: the victim may not be the object being written to

The value observed on a corrupted header is `0x0000_0200_4xxx_xxxx`, and this
page says in its own words that **this is the heap's own address range**. So the
writer is storing a *managed heap pointer* — a reference — at **offset 0** of a
live object.

An ordinary reference store into object `O` at index `i` writes at
`O + 16 + 16i`. For that to land on offset 0 of the victim, the writer's base
must sit **exactly 16 bytes below the victim** — which is what the fifth pass
already recorded, once, by hand:

> the words just below the victim decode as a plausible header —
> `class_id=1202 num_slots=11` at `base - 16` — **for an object the registry
> does not contain**.

That reframes the whole question. It is not necessarily "who corrupts the
victim's header". It is equally consistent with **an unregistered object based
at `victim - 16` whose field 0 IS the victim's header word** — i.e. two objects
overlapping by one header, with only one of them registered. Every audit that
asks "is the *receiver* a registered base?" is silent on it, because the store
is a perfectly ordinary store into whatever the writer believes it owns.

## What that makes the next instrument — **BUILT 2026-08-18**

`ZgcRealHeap::decode_neighbour_below` now runs inside the extent census and adds
two fields to the line it already prints:

```
below16 = plausible=true registered=false covers=true class=1202 size=192
below32 = plausible=false registered=false covers=false class=0 size=0
```

**`covers=true registered=false` is the second reading confirmed** — the victim
is not being corrupted, it is *overlapped*, and the "corrupting write" is that
object's field 0. `covers=false` on both leaves the stray-writer reading standing.

The census already printed the raw words below the base (`below0` / `below1`);
what it never did was say what they mean. That is the whole change: the fifth
pass decoded them once, by hand, on one capture, and the answer reframed the
defect — so it should not depend on someone thinking to do it again.

The negative half is what makes it worth reading: a properly adjacent
predecessor must report `covers=false`, or the field fires on every object in a
healthy heap and says nothing on the run that matters. That is asserted, and
verified by making `covers` unconditionally true.

### The original description follows

The fifth pass's `base - 16` probe was done once, manually, on one capture. Make
it automatic: **when the extent census reports a victim, also decode
`victim - 16` and `victim - 32`** and report, for each, whether it is a
plausible header (`alloc_size` succeeds), whether the registry contains it, and
whether its computed extent covers the victim. Three fields on a line that is
already being printed.

That distinguishes the two stories on the first crashing run:

* **a stray writer** — nothing plausible sits below the victim;
* **an overlapping allocation** — a plausible, unregistered object at
  `victim - 16` whose extent covers the victim, and the "corruption" is its
  field 0.

If it is the second, the question becomes "how did an object get based 16 bytes
below a registered base", and this page's own note that **the slide's
`registry.insert(*to)` is deliberately un-audited** is then the first place to
look, not the last.

## The elimination table, sixth pass

| candidate writer | verdict | the number / the reason |
|---|---|---|
| a bad registry insert (three shapes) | **no** | all 0 |
| the slide itself | **no** | post-slide survey clean the cycle before |
| the live set arriving broken | **no** | pre-slide census 0 |
| a retained TLAB chunk | **no** | `tlab_retire_skipped=0` |
| an allocation sized wrong | **no** | `zgc alloc audit` never fires |
| a Java field/array store | **no** | `zgc access audit` never fires |
| a raw native copy into the heap | **no** | `CRATONVM_DBG_HEAPCOPY` 0 |
| a write through a TAGGED arena handle | **no** | bounds-checked into its own `Vec`; the prescribed probe cannot fire (fifth pass) |
| **`System.arraycopy`** | **no** | per-element `set_array_element` — inside the audit |
| **`Unsafe.copyMemory` off-heap→heap** | **no** | refuses reference arrays; bounds-checked twice |
| a raw pointer TRANSLATED out of the arena | **untested** | `unsafe_arena_translation_stats` (fifth pass) |
| **an unregistered object based at `victim - 16`** | **instrumented, unrun** | `below16=` / `below32=` on the extent census line — `covers=true registered=false` confirms it |

# Seventh pass, 2026-08-18: the instruments are ARMED AND SILENT, and the crash did not reproduce in 52 reps

## The run

`io.netty.util.ResourceLeakDetectorTest`, one class per VM through
`netty-suite-runner`'s `CratonRunner`, `-XX:+UseZGC --nojit`, `--Xmx 1500m`, real
JDK, on Windows. **26 reps with `CRATONVM_DBG_ZGC_CORPSE=1` and 26 without.**

| arm | reps | SIGSEGV | overlaps | `below16` lines | arena warnings |
|---|---:|---:|---:|---:|---:|
| corpse gate ON | 26 | **0** | **0** | **0** | **0** |
| corpse gate OFF | 26 | **0** | — | — | — |

All 52 reported `found=3 started=3 ok=2 failed=1` — the counts this page records
for G1, i.e. the residual GC-independent failure and nothing else.

**The second arm exists because the first one is not enough.** An instrument that
runs on only one arm is a variable of the comparison: the corpse gate adds a full
registry survey twice per cycle, which changes timing and could perturb a racy
defect away. It did not — the uninstrumented arm is equally clean.

## The arm is NOT vacuous, and that is checked rather than assumed

This page's own history is a premature closure on `0/12`, so the first question is
whether the arm can crash at all. Compaction is **required** — this page's own
measurement is that `CRATONVM_ZGC_RELOCATE=0` has never produced an overlap or a
crash on any arm. So: does the arm compact?

| cycle | relocation on (default) | `CRATONVM_ZGC_RELOCATE=0` |
|---|---:|---:|
| 1 | 1,193,410,592 | 1,192,362,016 |
| 2 | **138,121,840** | 1,191,852,304 |
| 3 | 42,177,136 | 316,389,280 |

`cursor=` from the `[GC] zgc-reclaim:` line. **The cursor collapses on cycle 2
only when relocation is enabled** — that is a slide, and it does not happen with
the kill switch set. The arm also collects three times per run, freeing 1.17 GB
on cycle 1 with `sweep_us=590,926`. It collects, it slides, and `--nojit` means
relocation is permitted on every cycle rather than declined on 64 of 68.

## What that is worth, stated as arithmetic

At the documented rate of **3/23 ≈ 0.13**, the chance of 52 consecutive clean
reps is `0.87^52 ≈ 0.00075` — **under 1 in 1,300.** So the crash does not
reproduce at anything like its recorded rate on current `dev`.

## And it is still NOT RETIRED

Three reasons, and the first is this page's own scar tissue:

* **Nobody identified the writer.** Twelve candidates have been eliminated and
  none confirmed. A defect that stops reproducing without its cause being found
  has not been fixed, it has been perturbed — and the JIT-half of this pair is a
  worked example: it masked this one for weeks by declining relocation.
* **Both new instruments read ZERO, which is untriggered rather than
  disproven.** `below16=` never printed because no overlap was found to print it
  beside; the arena translation audit never warned. They are now *armed and
  silent* — a different and better state than "unrun", but not evidence about the
  two rows they were built for.
* **Many GC changes have landed since 2026-08-15**, several on 2026-08-18 alone
  and from several sessions. Any of them may have closed it. Nobody has bisected
  it, and the honest record is "does not reproduce", not "was fixed by X".

## The control was RUN, 2026-08-18: positive, but on different frames

Built `dev` at **`7eab6d6c2`** — the commit immediately before
`fix/zgc-slide-origin`, i.e. the tree the 3/23 belongs to, before the cursor
check that took it to 1/14 — and ran the same 26 reps on the same machine, same
harness, same `common.args` (the classpath points at the main worktree's netty
build, so the Java side is held constant and only the VM binary differs).

| tree | reps | SIGSEGV |
|---|---:|---:|
| `7eab6d6c2` (pre-fix, the 3/23 tree) | 26 | **2** |
| current `dev` | 52 | **0** |

**So the environment is exonerated.** This machine reproduces a ZGC `--nojit`
SIGSEGV on this class at ~2/26, and current `dev` produced none in twice as many
reps. At the control's own rate the chance of 0 in 52 is `0.923^52 ≈ 1.6%`; at
the page's 3/23 it is 0.075%. **The difference is in the tree, not the box** —
which is what the 52-rep run could not say on its own.

### But it is NOT demonstrably the same crash

Both control crashes carry this page's signature —
`jit: guarded compiled frames live process-wide: no (quiescence depth=0)`,
`0 compiled code range(s)` — so no compiled frame is involved, as documented.
Symbolized against the control binary, though, the frames are:

```
cratonvm_native_collections::chm_collect_all_entries
cratonvm_native_collections::map_state
```

**not** `reference_slots` / `relocate_stw`, which is what this page's fault
records. Two readings, and this page's own text supports either:

* **the same corruption, a different reader** — it already says "an unwalkable
  rewrite target is one route to the SIGSEGV and not the only one", and a
  corrupted header faults whoever walks it first;
* **a second defect** on the same workload and the same arm.

So the control proves the *tree* changed, and does **not** prove the documented
rewrite-pass fault is what changed. A bisect is now justified — and its first
duty is to **record which frames each crash symbolizes to**, or it will merge two
defects into one answer.

## Bisect, step 1 — and the step size is wrong

Rather than a blind midpoint over 108 GC-touching commits, the first step tested
a commit with prior evidence: **`175dc1751`**, *"the proxy Method cache was rooted
but never remapped"* — a cache holding pre-slide addresses the slide moved, which
is this defect's shape exactly.

| tree | reps | SIGSEGV |
|---|---:|---:|
| `7eab6d6c2` (pre-fix control) | 26 | **2** |
| `175dc1751` (the remap fix) | 26 | **0** |
| current `dev` | 52 | **0** |

So the closer is **provisionally** in `(7eab6d6c2, 175dc1751]`.

### Why "provisionally", and this is the important part

**A 26-rep clean step is not evidence.** At the control's measured rate of
2/26 ≈ 0.077:

```
P(0 crashes in 26 | p = 0.077) = 0.923^26 = 0.125
```

**One step in eight will read clean when nothing changed.** A bisect built on
26-rep steps therefore makes a wrong call at roughly that rate, and every step
after a wrong call searches the wrong half — which is exactly the failure this
page already has on its record, where a `0/12` closed the case and the reopening
noted "a ~1-in-10 event and zero in twelve draws are entirely compatible".

For 95% confidence that a step is genuinely clean, at this rate:

```
0.923^n < 0.05  ->  n >= 38 reps per step
```

At ~90 s per rep that is ~1 hour of running per step **plus** a ~20 minute build,
over log₂ of the ~124 commits still in range — call it 7 steps, so **the honest
price of this bisect is 8–9 hours**, not the ~4 estimated before the rate was
known. The `0/52` on current `dev` is the one figure here that clears the bar
(P = 1.6%); the two 26-rep readings do not, on their own.

**Do not narrow the range further on 26-rep steps.** Re-run `175dc1751` at n ≥ 38
before trusting the bracket above.

# CLOSED 2026-08-19: the writer was the collector's OWN reference processing

## The bisect landed on an adjacent pair

| commit | reps | SIGSEGV |
|---|---:|---:|
| `2fac8c241` = `aa4bc7922^` | 38 | **3** |
| **`aa4bc7922`** | 38 | **0** |

Parent and child. `aa4bc7922` is
*"fix(gc/refs, locale): retire two H2 known-issues — ReferenceQueue exclusion +
reference-processor shape guard"*, and it is the only commit in the final
13-commit bracket that touches GC-relevant code:
`vm/src/runtime/interpreter/gc_and_alloc.rs` (+169) and
`native-builtins/src/reference.rs` (+86). Neither is under `gc/`, which is why a
`git log -- gc/` filter showed the bracket as containing *no* GC change at all.

## The mechanism, in that commit's own words

> the null array was a `java.lang.String` value slot, **written by CratonVM's own
> reference processing through an address that had been reclaimed and reused**.
>
> `process_references_after_gc` and its G1-remark twin tested **only
> `num_fields >= 2` before writing through a pre-GC address**; a String has four
> and passes. … **This is what published a String as a queue head.**

And in the diff: `// num_fields >= 2 was the only shape test the two loops had`,
replaced by real `is_reference_shaped` / `is_cleanable_shaped` class checks.

**That is the writer this page hunted for seven passes**, and it explains every
observation, including the ones that made it look impossible:

| observation | why this writer produces it |
|---|---|
| an **8-byte heap pointer** at a slot | it writes a queue-head *reference* |
| `w0 = 0x0000_0200_4xxx_xxxx` | that is the heap's own address range |
| `zgc access audit` = **0** on overlapping runs | it is not `set_field` / `set_array_element` — the collector writes directly |
| `CRATONVM_DBG_HEAPCOPY` = **0** | not a raw native copy either |
| the three registry-insert checks = **0** | the allocator and the registry are innocent; nothing was inserted |
| `CRATONVM_ZGC_RELOCATE=0` **never** reproduced | without a slide the pre-GC address is still correct, so the write lands where it was meant to |
| `seen_at_slide_exit=true, size_at_slide_exit=96`, header now a pointer | the object at that address **moved**; the stale write then landed on whatever occupied it |
| `--nojit` required | the JIT arm declines relocation on 64 of 68 cycles, so it barely slides |

The one candidate the elimination table never had a row for was **the collector
writing through its own stale address** — and every audit on this page was built
to watch *mutators*.

## What is proven, and what is inference

**Proven by measurement:** `2fac8c241` crashes 3/38, `aa4bc7922` is clean 0/38,
current `dev` is clean 0/52, and the pre-fix control `7eab6d6c2` crashes 2/26 on
the same machine and harness. The fix commit is identified.

**Inference:** that the *documented* `reference_slots` / `relocate_stw` fault is
the same defect. What reproduced here symbolized to
`chm_collect_all_entries` / `map_state`. Both are readers of a heap whose headers
have been overwritten, and this writer overwrites headers — but the specific
frames on this page were never reproduced, so "same corruption, different reader"
is the reading, not a measurement. `reference_slots` was always one route to the
SIGSEGV and this page said so from the second pass.

**Retired on that basis.** Both instruments added on 2026-08-18 stay in the tree
and read zero, which is now the expected state rather than an open question.

## A methodological correction worth keeping

**Midpointing `git rev-list A..B` is not a bisect on a merge-heavy history.** One
step picked `68d47e766`, which turned out to be on a **parallel branch** —
`merge-base --is-ancestor` says it and `c69ad84d9` are ancestors of neither. Its
3/38 was a real measurement of a real commit, but it could not narrow anything,
because it does not lie between the ends. `git bisect` computes the commit that
best splits the *reachable set* precisely to avoid this; hand-rolled midpointing
does not.

The bracket was re-derived from ancestry, and the answer came from an **adjacent
parent/child pair**, which is immune to the error: no midpoint arithmetic is
involved in comparing `X^` with `X`.

Cost: 8 builds, ~9 hours of running, 6 measured commits — which tracks the 8–9
hours priced in step 1 once the crash rate was known.

## Bisect log — 2026-08-19

Every "clean" below is **38 reps**, the power step 1 derived
(`0.923^38 = 0.047`). Every arm is the same machine, same harness, same
`common.args`; only the VM binary differs. Newest first.

| commit | date | reps | SIGSEGV | verdict |
|---|---|---:|---:|---|
| current `dev` | 08-19 | 52 | 0 | clean |
| `175dc1751` (the remap suspect) | 08-18 | 26 | 0 | clean (underpowered, moot) |
| `9389c4256` = `175dc1751^` | 08-18 | 38 | 0 | **clean** |
| `829171d20` | 08-17 | 38 | 0 | **clean** |
| `b447e775e` | 08-17 | 38 | 0 | **clean** |
| `2fe85641d` "ZGC: genuine concurrent marking" | 08-16 | 38 | 0 | **clean** |
| `7eab6d6c2` (the control) | 08-16 | 26 | **2** | **crashes** |

**Bracket: `(7eab6d6c2, 2fe85641d]` — 57 commits**, and every one of them is
2026-08-16. Next pivot `c69ad84d9`.

Cost so far: 5 builds and ~5 hours of running. Each step is ~20 min of build plus
~57 min of reps, and the remaining ~6 steps put the total near the 8–9 hours
step 1 priced.

### Read this table for what it does NOT say

Four consecutive clean 38-rep readings are four independent 4.7% risks, so the
chance that *at least one* of them is a false clean is about **18%**. A single
wrong "clean" sends every later step into the wrong half. If the bisect lands on
a commit whose diff cannot plausibly explain the crash, **that is the signal to
re-run the nearest clean arms at higher n**, not to invent a mechanism for it.

## Bisect, step 2 — the suspect is exonerated, at full power

Step 1's suspect was `175dc1751`, *"the proxy Method cache was rooted but never
remapped"*, chosen because a cache of stale pre-slide addresses is this defect's
shape. **Its parent is already clean**, so it cannot be the fix:

| tree | reps | SIGSEGV | verdict |
|---|---:|---:|---|
| `7eab6d6c2` (pre-fix control) | 26 | **2** | crashes |
| `9389c4256` = `175dc1751^` | **38** | **0** | clean, at full power |
| `175dc1751` (the suspect) | 26 | 0 | clean (underpowered, now moot) |
| current `dev` | 52 | 0 | clean |

38 reps is the number step 1 derived: `0.923^38 = 0.047`, so this "clean" clears
the 5% bar the 26-rep readings did not. **The mechanism hypothesis is dead** — the
crash was already gone before the remap fix landed, so whatever closed it is
earlier.

Bracket: **`(7eab6d6c2, 9389c4256]`**, and note the range is **904 commits** by
`rev-list` (many are merge-ins from parallel branches, so the GC-touching subset
is far smaller — 124 by the earlier `-- gc/ native-collections/` count). Next
pivot is `829171d20` (2026-08-17).

### What this step is worth as method

It cost one build and one hour, and it **killed** a hypothesis chosen on
mechanism rather than position. That is the cheaper outcome: a positional
midpoint would have narrowed the range by half and told nobody anything about
*why*. Choosing pivots by mechanism keeps the option of a short-circuit, and
loses nothing when it misses — the range still halves.

## The control this run WAS missing, and why it mattered

The arm was proven non-vacuous — it collects and it compacts, checked above. **The
environment was not.** Nobody has shown that *this machine* reproduces the crash
at all, and the two are different claims: a non-vacuous arm says the code path
runs, not that the failure is reachable here.

The 3/23 was measured on a different day and possibly a different box. So the
honest reading of 0/52 is **"does not reproduce here, now"**, and it becomes
"the tree changed" only after this control:

> Build `dev` as of 2026-08-15 — the tree the 3/23 was measured against — and run
> the same 26 reps on the same machine.

* **~3 crashes** → the box reproduces it, something since then fixed it, and a
  bisect is worth its cost.
* **0 crashes** → the difference is the environment, not the tree, and nothing
  here says anything about whether the defect is gone.

Cost of the bisect *after* a positive control: each step needs ~26 reps to
separate a 0.13 rate from 0, so ~30 minutes per step plus a build, and log₂ of
the commits since 2026-08-15.

**What would retire it:** the control above coming back positive, then a bisect
across the commits since 2026-08-15 that finds the one which stops it, or a longer run at higher rep count that reproduces it
once with the instruments armed — at which point `below16=` answers the question
that has been open since the second pass. The instruments are in the tree and
cost a branch when off, so the next run is cheap.

# Eighth pass, 2026-09-01: the last elimination-table row, FIXED rather than probed

The bisect closed the crash on 2026-08-19. It left one row of the elimination
table open — **a raw pointer TRANSLATED out of the arena, `untested`** — and the
fifth pass had already said what closing it would take:

> Zero does *not* clear the path, because the unbounded-write half leaves no
> trace here; that half needs the bound to be carried to the callee rather than
> dropped, **which is a fix rather than a probe**.

That is done. Reading the path to fix it turned up **three** defects, one of them
worse than the one being looked for, and it also turned up two reasons the
fifth-pass instrument could never have answered the question it was built for.

**None of them is this page's writer.** The writer is proven: the collector's own
reference processing, `aa4bc7922`, and nothing here revises that. An arena block
is a Rust-owned `Vec<u8>`, not the managed heap, so this path could not have put
a pointer on a Java object's header. These are latent hazards in the same
neighbourhood, fixed on their own merits.

Branch: `fix/zgc-arena-ptr-bounds-20260901`.

## 1. The bound was dropped at the JNI boundary — carried now

`GetDirectBufferAddress` and `GetDirectBufferCapacity` **are** the bound: the JNI
contract is that a native may touch `capacity` bytes from the address. This VM
resolved the two independently and checked nothing, so a tagged handle whose
arena block was shorter than the advertised capacity was published anyway, and a
native following the contract wrote past the block. `unsafe_arena_real_ptr` had
always returned how many bytes were left; the call site discarded it
(`.map(|(ptr, _len)| ptr)`).

The address getter now goes through a new `unsafe_arena_real_ptr_bounded` and
answers **NULL** on a short block. NULL rather than a clamp, because the capacity
getter reads a Java field the address getter cannot correct, so the only
self-consistent pair on offer is `(NULL, capacity)` — and NULL is what the spec
already reserves for "not a direct buffer", hence what natives that check
anything check for. Both getters now resolve the capacity through one
`dbb_capacity`, so they cannot drift apart again.

`jni_long_arg_bits` — the other translating caller, netty-tcnative's
`SSL.bioWrite(long bio, long address, int len)` — **cannot** be bounded: a
`jlong` argument carries no length. It stays counted rather than guarded, and
that is stated here rather than left to be rediscovered.

## 2. A resize moved a block out from under an outstanding pointer

`Vec::resize` reallocates when it must grow past its capacity, and a
reallocation moves the bytes. Every real pointer handed out before that point
then named memory the process allocator had taken back — an unbounded write from
native code into whatever landed there next, which is exactly the shape this page
spent seven passes hunting.

The displaced buffer is now **retained**, keyed by whatever handle the block ends
up at and dropped when that handle is freed. The stale pointer reads
stale-but-mapped bytes instead of corrupting a stranger. **That is a downgrade,
not a cure** — the native is still using an address the Java side has moved on
from — but it bounds the blast radius to the block itself, and
`stale_on_realloc` still reports every occurrence. Retention is at most one
buffer per handle, so an allocate/translate/resize/free loop does not leak.

### And the fifth pass's instrument was armed where it could not fire

`try_reallocate`'s capacity probe ran `try_reserve_exact` on the **live** `Vec`.
That reallocates. So the buffer moved during the *probe*, and the detection
downstream then compared the new size against a capacity that had just been
satisfied and concluded nothing had moved. `stale_on_realloc` would have read
zero on a workload where the hazard fired on every resize.

The probe is gone; `reallocate` reports failure with `0` and `try_reallocate`
maps that to `None`.

## 3. Found in passing, and the serious one: a grown block swallowed its successors

`try_allocate` bumps `next_addr` by the **rounded request**, so each block owns
exactly that much handle space, and `locate` resolves an address to the greatest
base at or below it. `reallocate` grew `bytes.len()` and bumped nothing.

So a 64-byte block grown to 4096 **answered for every handle issued after it**.
Two live `Unsafe.allocateMemory` allocations, one address range, and every read
and write through the later handle landed silently in the earlier block. No
diagnostic anywhere, on either side.

`Arena` now records the span it reserved, and a grow that will not fit is served
from a **fresh handle**. That is `realloc(3)`'s contract, it is what
`Unsafe.reallocateMemory` already documents, and
`native_unsafe_reallocate_memory_consolidated` already passed the arena's answer
straight back to Java — its own comment said "the arena may have moved/resized
under this address".

This was found because it made the new unit tests flake: they resize to 1 MiB,
and the grown block then swallowed sibling tests' blocks in a process-global
arena. **A flaky new test was the defect reporting itself.**

## Measured against HotSpot, one probe, three binaries

`UnsafeShadowSweep` is the L1 differential probe, so the row belongs in it — two
live allocations staying independent is behaviour, not an address diff, which is
that probe's own rule. JDK 25.0.3+9, Windows, same class file on every arm:

| arm | `a grown block is not overwritten by a later allocation` |
|---|---|
| HotSpot | `true` |
| CratonVM, stock `dev` | **`false`** |
| CratonVM, this branch | `true` |

**And the whole-probe diff says nothing else moved.** 472 rows against the
HotSpot transcript: stock `dev` differs on 10 rows, this branch on 9, and a
direct diff of the two CratonVM transcripts shows **exactly one changed line** —
this row. The other nine are the L1 residuals that page already adjudicates.

### The row that would have hidden it

The companion row, `and the later allocation keeps its own bytes`, reads `true`
on **all three arms including the broken one** — the write and the read hit the
same aliased cell. A probe with only that row is a green light over live memory
aliasing. The damage is visible only from the *earlier* block, and only by
scanning all of it, because the later block lands somewhere in the middle.

## The instrument had no reader at all, and then had the wrong one

`unsafe_arena_translation_stats` was added in the fifth pass and **never called**
outside its own tests. So `translations` — the denominator — never reached a run,
and "the hazard never fired" and "no pointer was ever handed out on this
workload" were the same run. The `stale_on_*` warnings fired into `tracing`, with
nothing to read them against.

It now prints as one greppable line:

```
[VM] arena-ptr: translations=N short_translations=N stale_on_realloc=N \
     retained_on_realloc=N stale_on_free=N
```

with the GC summary, **and unconditionally whenever a stale or refused
translation has happened** — gating a correctness event behind a statistics flag
turns "nobody asked" into "nothing happened".

**And then the repro proved that was still the wrong place.** `CratonRunner` —
this page's own harness — calls `System.exit(1)` when a test fails, which leaves
through `lang_system`'s exit native and never reaches `vm-cli`'s normal-return
arm. The line printed on the `found=0` run where nothing happened and was
**absent from the `found=3 ok=2 failed=1` run that actually exercised the VM**.
`lang_system.rs` already carries the fix pattern with the reason beside it: the
corrupt-cell census moved to that path after "a 1975-class sweep produced this
line in zero logs and read as a clean run". The printer moved there too.

That is three instruments on this page armed where they could not fire —
`below16=`, `heapcopy_dbg()` behind the tagged early return, and this one twice.
It is the page's most reusable lesson.

## The aliasing defect has a real caller, and it is netty

`Unsafe.reallocateMemory` is not a theoretical path here.
`io.netty.util.internal.PlatformDependent0` line 727 —
`reallocateDirectNoCleaner`, behind `UnpooledUnsafeNoCleanerDirectByteBuf`'s
`capacity(int)` — is:

```java
return newDirectBuffer(UNSAFE.reallocateMemory(directBufferAddress(buffer), capacity), capacity);
```

**It uses the returned address**, which is the contract and which is why the
relocation in §3 is safe for it. Before the fix, growing such a buffer returned
the SAME handle and left the block covering every `Unsafe` allocation made after
it, so any later off-heap allocation in the process was silently aliased into
netty's grown buffer. Every read and write through the later handle landed in
the wrong block, with nothing reported on either side.

The JDK's own `DirectByteBuffer(int cap)` cannot trip the §1 bound either:
it allocates `max(1, cap + (pageAligned ? ps : 0))` and sets `address` to `base`
(or to `base + padding` when page-aligning), so the bytes remaining from the
published address are always `>= capacity`. A slice or duplicate carries a
smaller capacity at a higher offset, which is the same inequality. The guard is
therefore expected to read `short_translations=0` on healthy workloads, and that
is the reading to expect rather than a suspicious one.

## What has NOT been re-measured, and why

The repro itself — `io.netty.util.ResourceLeakDetectorTest` under
`-XX:+UseZGC --nojit`, at the 38 reps this page's own power calculation
derived — has not been re-run against this branch at the time of writing. Two
things stood in the way and both are worth recording:

* **the netty test build was gone.** `/data/cratonvm/apps/netty/*/target/` had
  been cleaned when `/data` filled, while `common.args` still listed those
  directories and the SNAPSHOT jars (main classes only) stayed put. A first
  38-rep arm therefore ran to completion with `rc=0`, `found=0`, ~22 s per rep
  and `collections=0` — a **vacuous arm that reads exactly like a clean one**.
  Rebuilding one module fixes it in about a minute:
  `mvn -o -pl common -am -DskipTests -Dcheckstyle.skip=true ... test-compile`,
  after which the documented `found=3 ok=2 failed=1` shape returns. Screen every
  rep on that shape, never on `rc`;
* **the shared host could not link the binary.** Fat LTO at `-j 1` was
  SIGKILLed twice at `avail` 0-4 GB with load 40-190 from other sessions. A
  driver is parked there waiting for `avail>=10G` and `load<30`.

What that leaves is stated plainly: the branch is verified by 4190
`native-builtins` and 2645 `cratonvm-vm` unit tests, 80/80 of the Java
regression suite against HotSpot, and the 472-row `UnsafeShadowSweep`
differential in which exactly one line moved. The GC crash this page is about
was closed by bisect on 2026-08-19 and none of this touches the collector. The
outstanding rep run would add a **denominator** — netty is the workload that
actually reaches `unsafe_arena_real_ptr`, so it is the one that can say whether
`translations` is non-zero in production — and it is not load-bearing for the
retirement.

## The elimination table, final

| candidate writer | verdict | the number / the reason |
|---|---|---|
| a bad registry insert (three shapes) | **no** | all 0 |
| the slide itself | **no** | post-slide survey clean the cycle before |
| the live set arriving broken | **no** | pre-slide census 0 |
| a retained TLAB chunk | **no** | `tlab_retire_skipped=0` |
| an allocation sized wrong | **no** | `zgc alloc audit` never fires |
| a Java field/array store | **no** | `zgc access audit` never fires |
| a raw native copy into the heap | **no** | `CRATONVM_DBG_HEAPCOPY` 0 |
| a write through a TAGGED arena handle | **no** | bounds-checked into its own `Vec`; the prescribed probe could not fire |
| `System.arraycopy` | **no** | per-element `set_array_element` — inside the audit |
| `Unsafe.copyMemory` off-heap→heap | **no** | refuses reference arrays; bounds-checked twice |
| an unregistered object based at `victim - 16` | **no** | `below16=` armed and silent across 52 reps |
| a raw pointer TRANSLATED out of the arena | **no**, and **FIXED anyway** | §1–§3: it cannot reach the managed heap, and the bound is now carried |
| **the collector's OWN reference processing** | **YES** | `2fac8c241` 3/38, `aa4bc7922` 0/38 |

The last row is the answer. The row above it is closed on two grounds worth
keeping apart: it could not have been this writer, and it was nevertheless a real
memory-safety defect — which is why it is fixed rather than argued away.

## What is proven, and what is not

**Proven by measurement:** the three defects above, each with its guard verified
in both directions. Every one fails with only its own fix reverted, and the
negative halves — a grow that fits keeps its handle; a resize that cannot move
retains nothing; a block that exactly covers its capacity is published — fail if
the guards are made unconditional. On the branch merged with `dev`: 4190
`native-builtins` unit tests, 2645 `cratonvm-vm` unit tests, and **80/80** of
the Java regression suite against HotSpot. The differential table above.

A note on that suite, because it bit twice in one session: `regression-suite`
and `UnsafeShadowSweep` both compile into a SHARED output directory, so two runs
at once produce `class not found` for whichever vector loses the race. A 78/80
and an earlier 79-with-`RReflect`-failing were both that, and both went to 80/80
when re-run alone. **Run it serially, or read its failures as your own.**

**Not proven:** that any of the three ever fired on a real workload. The
engagement counters exist now precisely so that question has an answer next time,
and this page's own history says a counter without a denominator is not an
answer.

**Still inference, unchanged from 2026-08-19:** that the `reference_slots` /
`relocate_stw` fault this page opens with is the same defect the bisect closed.
What reproduced symbolized to `chm_collect_all_entries` / `map_state`. Both are
readers of a heap whose headers have been overwritten, and the identified writer
overwrites headers — but the specific frames were never reproduced.

## Retired

The writer is identified and fixed, the last open row of the elimination table is
closed, every instrument this page built either reads zero for a stated reason or
has been given a reader, and the crash has not reproduced on `dev` since
2026-08-19.

## Related

- `zgc-resourceleakdetector-corpse-read-20260815.md` — retired; its closing
  section records why the JIT-on arm no longer reaches this defect.
