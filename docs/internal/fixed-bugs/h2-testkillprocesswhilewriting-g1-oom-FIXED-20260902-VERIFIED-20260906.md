# `-XX:+UseG1GC` fails `TestKillProcessWhileWriting` — FIXED 2026-09-02

**The class passes, 3/3.** Two independent defects, found in this order and
both measured: a conservative root pointing INSIDE a reference array, from
which the collector fabricated an object (section 4d), and one live
finalizable object disabling eager humongous reclaim for the whole process,
which left the heap 81% humongous and Eden at one region (section 4f). The
sections below are in the order they were investigated, so read 4d and 4f for
the causes and the `## Status` block for what each era of this page claimed.

## ADDENDUM 2026-08-30: the OOM face is FIXED, the 48 617 dangling references were never a rate, and the class still does not pass

Three separate corrections, in decreasing order of how much they change what
this page says.

### 1. The OOM face is fixed, and the fix is one gate that answered one bit too few

`plausible_mark_scan_target` returned a single bool, and ANY refusal set
`mark_saw_implausible`, which makes `cleanup` retain every region -- no
in-place frees, no humongous reclaim -- for the whole cycle. That is the chain
this page already traced: retain-all -> `humongous-eager declined_pauses=16103
spans=0 bytes=0` -> `degraded=empty-collection-set` -> a 1 MiB
`ByteBuffer.allocate` fails on a 1 GiB heap.

**Two conditions were reaching that one bit, and only one of them is evidence
of anything.** With the new `CRATONVM_G1_DBG_GRAY_PROV=1`, which records where
each gray-set entry was pushed from, a 32 s run says:

| refusal | count | what it is |
|---|---:|---|
| `NotAllocated` | **2 226** | at/beyond the region's cursor, or in a type that holds no object starts |
| `TornHeader` | 23 | inside the allocated prefix, header does not decode |

`NotAllocated` is provably not a live object: `cursor` only grows within an
incarnation, so every live object satisfies `off + size <= cursor`. And it is
ORDINARY, not corruption -- freed regions are deliberately no longer scrubbed
(G1AUD-10), so a recycled region still holds its previous incarnation's bytes
above the new cursor, and SATB retention means the marker legitimately scans
objects that died mid-cycle whose referents were freed with them. Every one of
the 2 226 names a live object's reference slot as its pusher
(`prov=scan-child-legacy<-0x20042fac8d8`), not a stale worklist entry.

The gate now returns `NotAllocated` vs `TornHeader`; only the latter impugns
the cycle. MEASURED, one binary, `CRATONVM_G1_MARK_OOB_FAILSAFE=1` as the
control, 900 s:

| arm | retain-all cycles | real OOM |
|---|---:|---:|
| failsafe restored (pre-fix behaviour) | 17 | 0 |
| split (default) | **1** | 0 |

and **0 real `OutOfMemoryError` across three default runs**, against this
page's 2 of 4. The `Caused by: java/lang/OutOfMemoryError: Java heap space
(ByteBuffer.allocate 1048576)` face is gone.

### 2. The 48 617 dangling references are SIX holders, and the count was never a rate

The V7b verifier walks every surviving region LINEARLY on a rotating budget, so
it re-reports the same unrepaired slot on every pause that reaches its region.
Deduplicated by `(holder, target)`, this page's 48 617 lines are **6 distinct
holders and 26 distinct targets**. The report now prints one line per distinct
pair and carries `distinct=` and `total=`, so the two numbers stay separable.

Two further cautions the page should carry:

* the verifier inspects DEAD objects too -- it walks the region, not the live
  set -- and a dead object pointing at a dead CSet object that was correctly
  not evacuated is not a UAF at all;
* `holder_marked` is now reported rather than assumed, and `marking_active` is
  reported beside it **because the first attempt at this datum was vacuous**:
  all 2 946 pairs in one run read `holder_marked=false` with
  `marking_active=false`, i.e. the bitmap was cleared and could not answer.
  A mark-bit read outside a mark cycle is not a liveness verdict.

### 3. THE FAILURE MODE MOVED, and that is a cost this page must carry

The retain-everything fail-safe was not fixing anything -- it was MASKING, by
never freeing. Removing the mask lets evacuation run again, and the corruption
that was previously latent now bites:

| arm | runs | outcome |
|---|---|---|
| this page's original default `-XX:+UseG1GC` | 4 | `124` cap, `1` OOM, `124` cap, `1` OOM |
| with the refusal split (default) | 4 | `124` cap, **`139` SIGSEGV**, `124` cap, **`139` SIGSEGV** |

Zero `OutOfMemoryError` and zero retain-all cycles in the new arm, and two hard
segfaults where there were none. **A caller should read that as a trade, not as
an improvement**: the class failed before and fails now, and a SIGSEGV is a
harder failure than a capped run.

It is shipped default-ON anyway, for two reasons that should be re-examined if
a G1 workload regresses. First, the blast radius is bounded -- G1 is not the
default collector, so only an explicit `-XX:+UseG1GC` arm is affected. Second,
the alternative is keeping a fail-safe that fires on an ordinary condition and
buys its silence by never reclaiming, which cannot be a resting state.

`CRATONVM_G1_MARK_OOB_FAILSAFE=1` restores the old behaviour on the same
binary, and `mark_oob_gray_skips` is reported at cleanup so the fail-safe that
stopped firing does not become one nobody can see.

### 4. The class still does not pass, and what remains is named

So the OOM face is gone and the corruption faces are not (see 3 above for the run
table and what that trade costs). The logs name the source outright, and
these are the lines to start from:

```text
[g1] worklist-scan[object]: REJECTED a non-object candidate (#134217728):
    holder=0x20045f1bf90 class_id=0 kind=Object slot=2200197358352
[g1] evacuation ref-scan CLAMPED a holder's element walk (#16384):
    obj=0x20045f1c140 declared=2162688 room=1971
```

Both are throttled to powers of two, so `#134217728` means **at least 2^27**
non-object candidates rejected and `#16384` at least 2^14 clamped walks. A
holder with `class_id=0` and a slot index of 2.2e12 is not an object; something
is walking memory that is not a live object and interpreting it as one. That is
the next question on this page, and it is upstream of both remaining faces.

### 4a. 2026-08-30 (later): the rejection report was naming the wrong field, and with that fixed the producer has ONE shape

`evacuation_candidate_is_an_object` takes `(holder, slot, raw)`. Its two ARRAY
call sites pass the element index for `slot`; its two OBJECT call sites passed
**`raw` for both**, so every rejection this family has ever printed carried a
heap address where the slot offset belongs. The object site is the one that
fires in the millions, so the whole population was unattributable -- the
`slot=2200197358352` in the line above is `0x2002FBF0BD0`, an address.

Fixed, and the report now also carries the holder's `num_slots`,
`array_length` and its region/offset/cursor, because `class_id=0 kind=Object`
alone reads identically for a real class 0 and for memory being walked as an
object. One 600 s run, every rejection:

```text
REJECTED a non-object candidate (#1): holder=0x20042800000 class_id=0
    kind=Object num_slots=8192 array_len=0
    holder_region=r4/Survivor/off=0/cursor=144312 slot=49616
    candidate=0x2004520df70
```

**Every one has the same holder shape** -- `class_id=0 kind=Object
num_slots=8192 array_len=0` -- and the holders sit at `off=0` of a Survivor
region (the first object the evacuator copied in) or at a fixed `off=300576` of
Old region 11 across cursors 470696 / 487824 / 590448 / 1048568.

That is the next question, and it is a narrow one: **what is a legacy object
with class 0 and 8192 slots, and is it an object at all?** `GAP_FILLER_CLASS_ID`
is `0xF111E701`, so it is not a TLAB tail filler; and `dbg_verify_reachable_
integrity`'s own `is_zeroed` predicate warns that a live never-hashed
`new Object()` is indistinguishable from reclaimed memory by `class_id` alone,
which is exactly the ambiguity the new `num_slots` field is there to break.
The candidates it yields (`0x2004520df70`, `+0x20`, `+0x18`, `+0x18`, ...) are
plausible heap addresses that are not object headers, so the slots are being
read as references either way.

### 4b. 2026-09-02: G1 published every out-of-line allocation BEFORE its header -- a real defect, and a CANDIDATE answer to 4a that the measurement does NOT confirm

Read this section for the defect it closes. It also offers an explanation of
section 4a's holder shape, and **section 4c measures that explanation and does
not confirm it** -- the shape survives the fix. Do not carry 4b's story forward
without 4c.

`G1Region::bump_alloc` advanced `self.cursor` FIRST and zeroed the span
afterwards, and all four of G1's header-writing allocation entry points --
`try_alloc_object`, `try_alloc_array` and the `GarbageCollector`
`alloc_object` / `alloc_array` -- wrote the `ObjectHeader` **after
`alloc_in_region` returned**, i.e. after the regions lock had been dropped.
The humongous path is the same shape with the region TYPE as its publication
point: it classified the span `HumongousStart` with `cursor = size`, then
zeroed, then returned for the caller to write the header.

The cursor is what every heap walk in `g1.rs` means by "there is an object
here" -- `scan_source_region_for_cset_refs`, `locate_in_object_grid` and the
Phase-4 walks all iterate `[0, cursor)` decoding a header at each step, and
`classify_candidate_header` accepts any address below it. So there is a window
in which an address is published and the bytes there are not yet a header.

**Read the layout and the shape falls out.** `ObjectHeader` is `class_id` and
`shape` in its first eight bytes; `kind` and `element_type` live in the top
bits of the mark word, in its SECOND eight. The intermediate state of an
ARRAY header store -- first half retired, second half not -- is therefore

| field | value in the window | why |
|---|---|---|
| `class_id` | 0 | a primitive array's class id IS 0 |
| `shape` | the array LENGTH | `num_slots` and `array_length` share this dword |
| `kind` | `Object` | the mark word is still the zeroed span; `ObjectKind::Object == 0` |
| `array_length()` | 0 | it returns 0 for any kind that is not `Array` |

which reads back as, verbatim, the censused holder:

```text
class_id=0 kind=Object num_slots=8192 array_len=0
```

A legacy object claims `num_slots * 16` body bytes. `int[8192]` owns 32 KiB and
claims 128 KiB; `long[8192]` and `Object[8192]` own 64 KiB and claim the same
128 KiB. `TLAB_MAX_ALLOC` is 32 KiB, so **every array larger than that took
this out-of-line path unconditionally**, and a TLAB miss sends smaller ones
down it too. That is also why the walk it drives could be evacuated: at 128 KiB
it is under the 512 KiB humongous threshold, so the collector copies it whole
into a fresh Survivor region -- landing at `off=0`, which is where the census
found it, and at a fixed offset of an Old region once promoted.

#### The default collector already had the invariant, which is the whole of the arm asymmetry

`GenerationalHeap::try_alloc_young_initialized` takes an initializer and its
SAFETY comment states the contract outright -- *"`init` writes the valid header
before the arena lock is released"* -- and every `gen_heap` allocation entry
point builds its `ObjectHeader` inside that closure. The JIT's inline allocator
states the same thing at length (`emit_inline_tlab_new`: *"no walker can ever
see a committed-but-unheadered object"*), and `Tlab::alloc_initialized`
implements it with a Release fence. **G1 was the only backend without it.**
This page's own control -- "the class passes under the default collector; only
the explicit `-XX:+UseG1GC` arm fails" -- is that difference.

#### What was fixed (independently of whether it is 4a's producer)

* `G1Region::bump_alloc_initialized` and
  `G1Collector::alloc_in_region_initialized` run the caller's header write over
  the fresh span while the regions lock is still held and **before** the cursor
  (or, for humongous, the region type) is published. All four callers now write
  their header there. Zeroing moved above the commit with it: a freed region is
  deliberately not scrubbed (G1AUD-10), so a walker arriving between the cursor
  bump and the memset read the previous incarnation's bytes.
* The two evacuation flat walks that also **WRITE** --
  `scan_and_evacuate_refs` and `scan_source_region_for_cset_refs` -- now take
  the region clamp `record_outgoing_rset_edges` already applied, with the
  `SLOT_SIZE` stride `holder_walkable_slots` had grown for exactly them and
  which no caller was using. Unclamped, those walks do not merely read past the
  holder: they rewrite `Value` cells past it with forwarded pointers. This is
  hardening, not the fix -- the censused holder's 128 KiB claim still fits
  inside its region, so the clamp would not have caught it.
* `evacuation_candidate_is_an_object` answered one bit where two are needed --
  verbatim the defect the 2026-08-30 `plausible_mark_scan_target` split fixed
  on the marking side. It now reports the `HeaderVerdict`, counts the TORN
  subset separately with its own throttle, and prints the holder's position in
  its own region's OBJECT GRID plus its raw mark word. `[GC] g1
  evac_ref_rejected=` carries the split.

### 4c. 2026-09-02, MEASURED: the OOM face stays gone, the control passes, and 4b is NOT this workload's producer

One binary (`8e9c0724`, `lto=false` -- a correctness run, not a timing one),
three arms interleaved, 900 s cap, `CRATONVM_GC_STATS=1`, Azure Linux
`--Xmx 1g`. Loads recorded because this host ran between 27 and 250 during the
window and the page has been misled by a contended arm before.

| arm | rc | secs | loadavg | real `OutOfMemoryError` | `[SECURITY V7b]` lines |
|---|---:|---:|---|---:|---:|
| A `-XX:+UseG1GC` | 124 (cap) | 901 | 27 -> 74 | **0** | 28 |
| B same + `CRATONVM_G1_LATE_HEADER_WRITE=1` | 124 (cap) | 900 | 74 -> **250** | **0** | 0 |
| C default collector | **0 (PASS)** | 811 | 250 -> 66 | 0 | 0 |

Three things this table does and does not say.

* **The control is not vacuous.** C passes in 811 s under a load excursion, so
  the 900 s cap is reachable on this host on this day; A and B capping is a
  failure, not a slow machine. That is the arm this page asserts and it now has
  a same-day, same-binary measurement.
* **The OOM face stays gone**, on both G1 arms, which is the 2026-08-30 fix
  holding rather than anything new here.
* **A vs B is NOT usable.** B ran through a load-250 excursion; per this repo's
  own rule a contended arm can invert a verdict, so the kill switch has not yet
  been exercised on comparable ground. It exists (`CRATONVM_G1_LATE_HEADER_WRITE=1`,
  one binary) so that comparison can be made on a quiet host.

The V7b line counts are **not** comparable to this page's 48 617 / 50 747: the
2026-08-30 addendum deduplicated that report to one line per distinct
`(holder, target)` pair. 28 and 0 are distinct pairs against six.

#### And the 4a holder shape SURVIVES the ordering fix

Arm A, with the allocation-ordering fix active, still produces it:

```text
REJECTED a non-object candidate (#1, torn=false torn_total=0 verdict=AboveCursor):
    holder=0x20042800000 class_id=0 kind=Object num_slots=8192 array_len=0
    holder_mark=0x1000000000000000
    holder_region=r4/Survivor/off=0/cursor=144904
    grid=OBJECT-START idx=0 size=0x20010 slot=49872 candidate=0x20044b0e070
```

The two new fields settle two things at once. `grid=OBJECT-START idx=0` says the
holder is the FIRST object in its region's own grid, not an interior address
someone mis-derived. And `holder_mark=0x1000000000000000` is `gc_age=1` and
nothing else -- an evacuated 8192-element reference array would read
`0x1001000000000000`, so this is that word **with exactly the kind bit (48)
missing**, and with the age bump present, meaning the evacuator wrote it.

A second run reproduces the same POSITION with different contents --
`class_id=1130142320 num_slots=512 size=0x2010` and `class_id=1160062808`, both
at `off=0` of a Survivor region, `idx=0`. So the invariant is not "class 0", and
it is not "8192": it is **the first object copied into a Survivor region has a
header that is not an object**.

#### What the source/destination split rules out

`evacuate_object` now snapshots the source's `(class_id, shape, kind)` and
compares the destination's after the copy and the two quartet updates. Over a
500 s run: **`copy_shape_drift=0`**. The memcpy and `set_gc_age` /
`add_gc_flags` are the only writes there, so the destination's garbage is
INHERITED, not manufactured -- which is what pushes the question upstream of
evacuation entirely.

`note_root_object_plausibility` is worth naming here because it looked like the
answer and is not: a CSet root that fails the object screen is counted and
**evacuated anyway** (`NON_OBJECT_ROOT_COPIED` exists precisely to log that).
Measured on the same run: `CSet ROOT is not an object` **0**, `a NON-OBJECT root
was COPIED` **0**. Roots are not the source on this workload.

#### A trap this page should not step in twice

The first source-side screen refused every `class_id >= 1 << 24` -- sound for
LOADED classes, wrong for this VM. `AUTOBOX_CLASS_ID` is `u32::MAX` and lambda
proxies are numbered by `SharedVm::alloc_lambda_proxy_id`, a bare counter from
`0x8000_0000`. It reported 18 implausible headers in one run and **all eighteen
were autobox wrappers or lambda proxies** -- a screen firing on correct,
unchanged behaviour, which is worse than no screen because it invites a
conclusion. The screen now refuses only the BAND between `1 << 24` and
`0x8000_0000`, plus class 0 carrying thousands of slots.

#### What is open

Who writes a header, at the start of an object the evacuator then copies into a
Survivor region, with the kind bit clear. Ruled out so far: the evacuation copy
itself (`copy_shape_drift=0`), non-object roots (0/0), and -- for this
workload -- G1's out-of-line allocator, whose window is real and now closed but
whose closure did not remove the shape. The next instrument is the same
source-side screen, corrected, run with `CRATONVM_G1_DBG_REACH=1` so the report
names the CARVE that handed out the span; and the A-vs-B kill-switch comparison
on a host quiet enough for it to mean something.

### 4d. 2026-09-02, ROOT CAUSE: G1 evacuated a CSet root that pointed INSIDE an array, and manufactured an object out of the element

Sections 4a-4c chased the holder shape through the collector and kept arriving
one move too late. The instrument that ended it prints, for an implausible
header, its position in its own region's OBJECT GRID and the raw bytes:

```text
IMPLAUSIBLE legacy header at cset-root (#1): obj=0x20045200130
    class_id=1135069736 kind=Object num_slots=512 mark=0x0 claims=0x2010 bytes
    source=r46/Survivor/off=0x130/... OWNER=evac:hint-dest extent=[0x108,0x218) size=0x110
    grid=INTERIOR of=0x108 delta=0x28 size=0x110 cid=185 kind=Array idx=4
    bytes[ ... >>0x130=0x0000020043a7ca28<< ... ]
```

**The root is 0x28 bytes inside a live reference ARRAY**, and the region's own
carve trail confirms the extent independently (`OWNER=evac:hint-dest
extent=[0x108,0x218) size=0x110`). `evacuate_object` read that ELEMENT as an
object header. The element holds a heap pointer, so:

| header field | what it actually read | value |
|---|---|---|
| `class_id` | the pointer's LOW half | `0x43a7ca28` |
| `num_slots` | the pointer's HIGH half | `0x200` = **512** |
| `mark_word` | the next eight bytes | 0 -> `kind=Object` |

`num_slots=512` is not a shape. **It is the top half of every address on a heap
based at `0x2_0000_0000`** -- which is why every holder in sections 4a-4c
reported 512, and why the `class_id` looked like random garbage each run: it is
whatever pointer that slot happened to hold. (The `class_id=0 num_slots=8192`
variant is the same read landing on a slot whose high half is 0.)

The object was then sized at `0x2010`, **eight kilobytes were copied into a
Survivor region**, and `*root` was rewritten to name the fabrication. Scanning
it as 512 legacy `Value` slots is the entire downstream family: the rejected
candidates, the CLAMPED walks, the dangling references and the segfault.

#### The guard had been reporting this for its whole life and doing nothing

`note_root_object_plausibility` already answered "is this CSet root an object?"
and `NON_OBJECT_ROOT_COPIED` already existed **to count the times the evacuator
copied one anyway**. The guard was measurement-only, and what it was measuring
was the collector manufacturing an object out of an array element.

#### ...but it was the wrong predicate, and the first fix measured ZERO

Refusing on that guard alone changed nothing: `skipped=0` on a run still
producing implausible headers. `candidate_header_is_plausible` asks whether the
two header tag bytes decode and the address sits below its region's cursor --
and **an interior address satisfies both trivially**, which is exactly why this
root was evacuated and why `NON_OBJECT_ROOT_SEEN` had read 0 all along. The
refusal has to be on BOTH predicates: the tag screen and the implausible-header
screen. That is the shipped fix; both root loops now leave such a root
unchanged and pin its region.

#### MEASURED: the cascade is gone

One binary, `-XX:+UseG1GC`, 900 s cap, quiet host (loadavg 4-14). Counts are
reported lines, and the report throttle is shared across sites, so read the
ZEROES rather than the ratios:

| implausible header at | before the fix | after |
|---|---:|---:|
| `cset-root` (the arrival) | 6 | 11 |
| `evacuate-src` | 2 | **0** |
| `evacuate-dest` | 1 | **0** |
| `worklist-holder` | 2 | **0** |
| `rset-source-walk` | 4 | **0** |
| `ref-slot-candidate` | 3 | **0** |

The bad addresses still ARRIVE as conservative roots -- that is what
conservative scanning is for, and 11 of them is not a defect -- but nothing
downstream is fabricated from them any more. `[SECURITY V7b]` dangling
references went to **0**, from 19-65 on the immediately preceding runs and
48 617 / 50 747 when this page was opened. `copy_shape_drift=0` throughout, and
`NON_OBJECT_ROOT_COPIED` is now structurally zero and still printed, so
reintroducing the copy shows up in the same line that reports the skips.

Three G1 reps and a same-day control, one binary, quiet host:

| arm | rc | secs | skipped | copied | rej | implausible | V7b |
|---|---:|---:|---:|---:|---:|---:|---:|
| `-XX:+UseG1GC` rep 1 | 124 (cap) | 900 | 11 | 0 | 19 | 11 | **0** |
| `-XX:+UseG1GC` rep 2 | 124 (cap) | 900 | 11 | 0 | 18 | 11 | **0** |
| `-XX:+UseG1GC` rep 3 | 124 (cap) | 900 | 12 | 0 | 20 | 12 | **0** |
| default collector | **0 (PASS)** | 403 | 0 | 0 | 0 | 0 | 0 |

Re-measured after moving the refusal before CSet selection (see below), 3 reps
plus control: `124 / 124 / 124` and `0 (PASS)` in 554 s, `skipped=0` throughout
because the roots no longer reach the root loop at all, `v7b=0` throughout.

The control passing in 403 s on the same host and binary is what makes the cap
a failure rather than a slow machine, and it is the arm this page asserts.

#### ...and the refusal belongs one layer EARLIER, before the collection set is chosen

Refusing in the root loop is a backstop. The mechanism that is supposed to keep
a conservatively-discovered root's region out of the CSet in the first place is
`pinned_region_set_including_non_object_roots`, which runs BEFORE selection --
and it gated on `candidate_header_is_plausible` alone, the same screen an
interior address passes. So the region joined the CSet, and the root loop was
the only thing left to catch it.

With both screens there (`addr_is_followable_object`), the region is simply
pinned out: nothing in it moves, the interior root stays valid, and the root
loop never sees it. Measured, 3 reps, quiet host:

| | root-loop fix only | pinned out before selection |
|---|---:|---:|
| implausible at `cset-root` | 11-12 | **0** |
| implausible at `root-pin-scan` | -- | 14-15 (with a matching "pinning region" line each) |
| roots SKIPPED in the root loop | 11-12 | **0** |
| `[SECURITY V7b]` | 0 | **0** |

The same commit also removed a pin the root-loop fix had introduced:
`G1Region::pinned` is the JNI-critical pin, cleared only by a matching
`unpin_region` or by `reset`, and a pinned region is never collected so it is
never reset. Setting it from the root loop leaked the region for the life of
the process -- and bought nothing even for the current pause, because the CSet
is already chosen by then.

#### What is NOT fixed

**The class still caps at 900 s** (`rc=124`, 3/3) while the default collector
passes the same workload in 403 s on the same host -- so G1 is at least 2.2x
off the control and may be livelocked. The corruption face is what
closed; the cap face is not, and nothing here should be read as claiming it.
`rej` stays around 18-19, of which all but one come from
`rset-source-scan[object]` -- the LINEAR walk, which visits dead objects as
well as live ones, so per this page's own section-2 caveat those are the
weakest evidence it collects.

### 4e. 2026-09-02: the CAP face is a whole-heap walk per young pause, feeding a reclaim that declines

The corruption face and the cap face are different defects. With the root fix
in, one 2400 s `--verbose:gc` run under `-XX:+UseG1GC` produced **32 MB of
`[GC-STAT]` lines and 169 205 YoungOnly pauses in 37 minutes** -- against a
default collector that finishes the whole class in 403-554 s.

Summed over those pauses:

| | mean per pause |
|---|---:|
| `pause_us` | **7 588** |
| `closure_us` (the evacuation itself) | **18** |
| `roots_us` | 129 |
| `rset_us` | 1 222 |
| **`fixup_us`** | **3 063** |
| `free_us` | 14 |
| `objects_copied` | 64 |
| `bytes_copied` | 9 888 |
| **`fixup_regions`** | **843** |
| **`fixup_bytes`** | **446 MB** |

Total stop-the-world pause time was **1 284 s of a ~2 220 s run** -- the
collector owns 58% of the wall clock. And the shape says where it goes: copying
64 objects takes 18 us, while the fix-up walk takes 3 063 us and covers **843
regions of a 1024-region heap -- essentially the whole heap, on every young
pause**, 169 205 times, for 76 TB walked in one run.

`phase4_regions_to_walk` exists precisely to narrow that walk, and one line
decides whether it may:

```rust
let want_census = gc_flags().g1_eager_humongous && heap_has_humongous;
```

`want_census` forces `phase4_regions_to_walk` to return `None` (the wide walk),
because "nothing in the heap references span H" is a whole-heap claim. Eager
humongous reclaim is default-ON, and H2's MVStore keeps 1 MiB `ByteBuffer`s
live -- humongous is anything over half a 1 MiB region -- so `heap_has_humongous`
is essentially always true on this workload and the narrowing NEVER APPLIES.

The sharp end: this page's own 2026-08-29 census recorded
`humongous-eager: spans=0 bytes=0 declined_pauses=16103`. **The whole-heap
census that costs 3 ms per pause is feeding an eager reclaim that declines every
time and frees nothing.** `CRATONVM_G1_EAGER_HUMONGOUS=0` is the one-flag,
one-binary A/B for that, and it is the next measurement this page needs.

Nothing here is a corruption claim, and none of it is affected by the root fix
in 4d -- it is the same shape the OOM face's `degraded=empty-collection-set`
chain was reported against in 2026-08-29, now priced.

#### The census is DISCARDED on every pause, and the reason is always the same

A `CRATONVM_G1_DBG_REACH=1` run settles what the census is FOR. Of 15 638
`[GC-STAT]` lines it produced 15 639 of these:

```text
[g1][HUMONGOUS] eager reclaim declined: an object registered for finalization
    is awaiting finalize()
```

**Every pause.** The first gate in `eager_reclaim_humongous_locked` is
`finalizer_pause`, and one registered, not-yet-finalized object holds it true
for the whole run -- so the whole-heap walk is paid for on every pause to build
a census that is thrown away before it is read. That is the 2026-08-29 line
(`spans=0 bytes=0 declined_pauses=16103`) seen from the other end.

Every gate that function declines on -- except `census.complete`, which is a
property of the walk itself -- is decidable BEFORE Phase 4. They now live in
one `eager_reclaim_early_decline` that both the reclaim and `want_census`
consult, so a census is not paid for when the reclaim is already going to
refuse it. The two cannot drift, which matters because a census paid for and
then declined looks exactly like a census that was needed.

The A/B that bounds the win, one binary, interleaved, 900 s:

| arm | mean `fixup_regions` | mean `fixup_us` | mean `pause_us` | rc |
|---|---:|---:|---:|---|
| `CRATONVM_G1_EAGER_HUMONGOUS=1` rep 1 / 2 | 845 / 841 | 3 764 / 4 178 | 8 682 / 9 704 | 124 / 124 |
| `CRATONVM_G1_EAGER_HUMONGOUS=0` rep 1 / 2 | **5 / 5** | 864 / 1 196 | 5 315 / 7 263 | 124 / 124 |

(The `=0` rep 2 ran through a loadavg-414 excursion, so read its TIMES with
suspicion; `fixup_regions` is structural and is not affected.)

#### MEASURED: the census skip reaches the off-switch's numbers with the feature ON

`want_census` turned out to exist as TWO expressions -- one inside
`update_references_in_regions` deciding whether the census is BUILT, one at the
`phase4_regions_to_walk` call site deciding the fix-up walk's WIDTH. The first
cut of the fix changed only the former and measured `fixup_regions=831`,
unmoved from baseline, because the call site still said "wide". A census
skipped while the whole-heap walk still runs is the worst of both. Both now go
through one `want_humongous_census`.

One binary, 900 s, interleaved, quiet host (loadavg 6-14):

| arm | `fixup_regions` | `fixup_us` | `pause_us` | pauses | rc |
|---|---:|---:|---:|---:|---|
| baseline, eager ON (before this fix) | 845 / 841 | 3 764 / 4 178 | 8 682 / 9 704 | 61 481 / 51 525 | 124 |
| `CRATONVM_G1_EAGER_HUMONGOUS=0` | 5 / 5 | 737 | 4 660 | 96 194 | 124 |
| **census skip, eager still ON** | **5 / 5** | **658 / 894** | **4 361 / 5 539** | 101 780 / 82 132 | 124 |

The fix reaches the off-switch's numbers WITHOUT turning eager reclaim off:
**the fix-up walk drops from 843 regions to 5 and the mean young pause halves,
8.7 ms -> 4.4 ms.** It does this only on pauses where the reclaim could not
have run anyway, so nothing that eager reclaim would have freed is given up.

### 4f. 2026-09-02, THE CAP FACE: one live finalizable object disabled humongous reclaim for the whole process

The rate had a cause, and it is the same gate section 4e found declining the
census -- but the cost is far larger than the wasted walk.

**The heap was 81% humongous garbage.** The per-pause region census (which had
to be repaired first -- it sat behind a `try_lock` on the regions mutex that
every collection path already holds, so it had never once printed) says:

| | mean per pause |
|---|---:|
| `hum_regions` | **829 of 1024** |
| `free_regions` | 185 |
| `old_regions` | 7.5 |
| `eden_regions` | **1.1** |
| `cset_regions` | **1.5** |
| `jit_pinned_out` | 1.3 |

Eden is ONE REGION. Every ~1 MB of allocation fills it, triggers a pause that
may collect 1.5 regions, frees 356 KB, and the next allocation triggers again --
80 young pauses per second. Pinning is not the cause (1.3 regions), so the
section-4d root fix is not implicated.

#### Why the humongous population never falls

Eager reclaim is the only thing that reclaims humongous spans, and
`eager_reclaim_humongous_locked` declined outright whenever `finalizer_pause`
was set. That flag is set for **ANY registered not-yet-enqueued finalizable
object**, not just a dead one -- one live `FileInputStream` is enough -- so on
this workload it never ran, on any pause, for the life of the process.

H2's MVStore allocates 1 MiB `ByteBuffer`s. Humongous is anything over half a
region, and `16 + 1048576` bytes needs TWO 1 MiB regions, so every buffer costs
2 MiB and none of them ever came back.

#### MEASURED, one binary, one flag

A deliberately UNSOUND probe (since removed) bypassed just that gate:

| | gate ON (default) | gate bypassed |
|---|---:|---:|
| young pauses / 900 s | 76 787 | **88** |
| `hum_regions` | 830.6 | **26.5** |
| `free_regions` | 183.6 | **980.3** |
| `cset_regions` | 1.2 | **128.1** |
| bytes freed per pause | 485 KB | **449 MB** |
| outcome | `124` (cap) | **`0` — PASS in 633 s** |

**872x fewer pauses, and the class passes** -- against 552 s for the default
collector on the same host and binary.

#### The shipped fix names the hazard instead of declining for it

The gate's own field doc states the hazard exactly, and it is narrow: *"a
humongous object with a finalizer ... never resurrected ... it would just be
freed out from under a `finalize()` that has not run yet."* A humongous span is
never in the CSet, so Phase 3.5 never resurrects it and it never reaches
`resurrected_finalizers`.

So `finalizer_addrs_this_pause` keeps the address list Phase 3.5 consumes, and
the reclaim marks each of those objects' spans live. Nothing else is needed: a
dead finalizable object that merely REFERENCES a humongous span is already
covered, because `HumongousCensus::referenced` counts references from dead
holders by design ("over-approximates liveness"), and a resurrected one is
walked at its post-copy address.

Two regression tests, one per direction. The second --
`an_unrelated_finalizer_candidate_no_longer_suppresses_eager_reclaim` -- FAILS
on the old code, so it is not a vacuous guard.

#### MEASURED on the shipped fix: the class PASSES

Three G1 reps and a same-day control, one binary, 900 s cap:

| arm | rc | secs | pauses | `hum_regions` | `free_regions` | `cset_regions` | V7b | implausible |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| `-XX:+UseG1GC` rep 1 | **0 PASS** | 694 | 89 | 27.7 | 978.4 | 127.8 | 0 | 0 |
| `-XX:+UseG1GC` rep 2 | **0 PASS** | 716 | 90 | 35.8 | 969.8 | 126.2 | 0 | 0 |
| `-XX:+UseG1GC` rep 3 | **0 PASS** | 605 | 90 | 34.2 | 971.6 | 126.5 | 0 | 0 |
| default collector | 0 PASS | 392 | -- | -- | -- | -- | 0 | 0 |

The sound fix reproduces the unsound probe exactly (88 pauses / hum 26.5 /
free 980.3 / 449 MB freed per pause), which is what makes the probe's result
transferable. `v7b=0` and `implausible=0` on every rep, so the section-4d root
fix holds under a collector that is now actually reclaiming.

G1 runs the class in 605-716 s against the control's 392 s -- still ~1.7x, which
is a throughput question and not this page's.

#### What is left


At the time 4e was written no arm passed -- halving the pause cost just bought
more pauses in the same 900 s (82 000 - 102 000, up from 51 000 - 61 000).
Section 4f found why, and with it fixed the pause count is 89. Turning the
fix-up walk off alone still left **tens of thousands of young pauses per 900 s** -- one every 11-15 ms, each freeing about
0.5 MB of a 1 GiB heap, against 88 Mixed pauses in 2400 s. `young_target_regions`
starts at 60% of the heap, so the young generation is not supposed to be
collected at that granularity. Why the trigger fires that often, and why Mixed
almost never runs, is the next question on this page.

## PLATFORM GAP 2026-09-04: the fix is verified on LINUX only; the Windows arm still fails

This page's `FIXED 2026-09-02 / passes 3/3` was measured on the Azure Linux host
-- its own `## Reproducing` block says so (`source /data/toolchain/env.sh`,
`cd /data/cratonvm/apps/h2database/h2`), and the 605-716 s pass and the 392 s
default-collector comparison are both from there.

**On Windows the class fails, and it fails at the SAME COMMIT that claims it
passes.** `-XX:+UseG1GC --Xmx 1g`, local H2 corpus, 3 runs per arm:

| build | results |
|---|---|
| `8287cf2b7` -- the merge titled "passes under `-XX:+UseG1GC`" | rc 127 / 124 / 1 |
| current dev + the ZGC branch | rc 127 / 139 / 1 |

Identical faces, identical timings, `FATAL: heap exhausted allocating
java/lang/String` on both `rc=127` runs. **There is no regression between them**
-- which is why no bisect was run over the 357 commits separating them: the
known-good endpoint was verified FIRST and did not reproduce its own claim here.

Three suspects were eliminated cheaply on the way, and are recorded so nobody
re-tests them:

| suspect | verdict |
|---|---|
| the ZGC branch's three new default-ON flags | exonerated -- 3/3 fail with all three forced off |
| dev's `feat/g1-inline-barrier-default-on-20260904` | exonerated -- 3/3 fail with `CRATONVM_G1_INLINE_BARRIER=0` |
| a dev regression since 2026-09-02 | refuted -- the known-good endpoint fails identically |

The failure face varies exactly as this page already warns: `rc` of 1, 124, 127
and 139 across twelve runs. Score only SIGSEGV as BAD if anyone does bisect
something here.

**What this means for the page.** The corruption and cap faces are fixed and the
Linux evidence for that stands; nothing here contradicts it. What is NOT
established is that the fix holds on Windows, where the class still exhausts the
heap. That arm is OPEN and this page cannot be retired until it is either
verified on Linux again (where the claim was made) or repaired on Windows.

Also worth reading before assuming the clamp closed everything: the Windows runs
emit `evacuation ref-scan CLAMPED a holder's element walk` repeatedly, and
`pause ran with a live compiled frame and an EMPTY JIT root publication:
pin_addrs=0 ... this pause evacuated against a root set it could not prove`. The
clamp is the 2026-09-01 fix working, but it firing this often means the
header-vs-region disagreement it contains is still happening.

## Status

**FIXED 2026-09-02 — the class PASSES under `-XX:+UseG1GC`, 3/3 (605-716 s, against 392 s for the default collector on the same host and binary), with zero dangling references and zero implausible headers.** Two independent defects had to close: the CORRUPTION face (section 4d — G1 evacuated a conservative root pointing INSIDE a reference array and fabricated an object from the element) and the CAP face (section 4f — one live finalizable object disabled eager humongous reclaim for the whole process, leaving the heap 81% humongous and Eden at one region). Historical status below.

**The OOM face is FIXED (2026-08-30) and held on 2026-09-02 (section 4c: 0 real `OutOfMemoryError` on both G1 arms, and the default-collector control PASSES in 811 s the same day, so the cap is a failure and not a slow host). The ROOT CAUSE of the corruption family is found and fixed (section 4d): G1 evacuated a CSet root pointing INSIDE a reference array and manufactured an object out of the element -- `num_slots=512` was the top half of a heap address, not a shape. Every downstream implausible-header site went to ZERO and V7b dangling references to 0, but the class STILL CAPS at 900 s, so the cap face is untouched. A separate allocation-publication defect was also fixed (section 4b) and did not close anything on its own. The FAILURE MODE MOVED to SIGSEGV in 2026-08-30's arm -- read section 3 before treating that as an improvement. The 48 617 dangling references are 6 holders, not a rate. Split out 2026-08-29** from
`bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`, whose ZGC
defect is closed and which never owned this row. The class **passes under the
default collector**; only the explicit `-XX:+UseG1GC` arm fails.

## ADDENDUM 2026-09-06: the SUITE cannot show this class passing, at any state of the fix

This page says the class passes in **605-716 s** under G1, and 811 s for the
default collector on the same host and binary. The H2 suite runner's default
per-class timeout is:

```
apps/h2database-suite-runner/run-h2-suite.sh:40
CLASS_TO="${CLASS_TO:-300}"          # per-class timeout seconds
```

**The class STRADDLES that cap, so the suite row flaps on host load alone.**
Re-measured 2026-09-06 on binary `a044e1fe1` (a dev ancestor carrying both
fixes), five runs interleaved on an idle box, load 4-7:

| arm | rc | seconds |
|---|---|---:|
| G1 | 0 | 377 |
| default | 0 | 224 |
| G1 | 0 | 266 |
| default | 0 | 299 |
| G1 | 0 | 360 |

**G1 3/3 PASS, default 2/2 PASS** — the fix holds, four days and 144+ commits
after it landed, with the default collector as its control.

Now put those against `CLASS_TO=300`: two of the three G1 runs (377 s, 360 s)
exceed it and one (266 s) does not. On the LOADED host this page's original
numbers came from it was 605-716 s and always exceeded it. So the same healthy
binary reports PASS or TIMEOUT for this class depending on nothing but how busy
the box is.

That is worse than a cap that always fires. A row that always times out gets
investigated; a row that flaps gets called flaky and dismissed, and the next
reader has no reason to suspect the cap at all.

That is not a hypothetical. It is the row
`zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md`
records as `store.TestKillProcessWhileWriting | CRASH | HANG` and reads as "no
longer crashes, still does not pass". The crash half is real progress; the HANG
half is this cap, and that page's own note that its HANG totals "are 300 s class
timeouts on a shared box, so read them as contention" is the same observation
one step short of the cause.

**Anyone re-running this class must raise the cap or they will re-open this
row.** Through the suite (`--only` is the class filter; there is no `--class`
flag, and `--class-to` is the same knob as `CLASS_TO`):

```bash
CLASS_TO=1800 ./run-h2-suite.sh run --category all --only TestKillProcessWhileWriting
```

1800 s is ~2.5x the slowest measured pass, which leaves room for a loaded host —
this box has run between load 3 and load 104 on 8 cores in a single night.

**The suite cannot select the G1 arm at all**, which is why this page's numbers
were never suite numbers: `-XX:+UseG1GC` is a vm-cli ARGUMENT
(`vm-cli/src/main.rs:4682`), the runner passes only `CRATONVM_*` env through to
the child, and `grep -E "UseG1GC|VM_OPTS" run-h2-suite.sh` returns nothing. Run
the class directly, exactly as `run_one_class` does (the `cd` into a scratch
workdir and the watchdog disable both matter — the latter stops a class that
legitimately takes the whole budget from abort()+dumping):

```bash
H2=/data/cratonvm/apps/h2database/h2
CP="$H2/target/classes:$H2/target/test-classes:$(cat $H2/craton-testcp.txt)"
mkdir -p /tmp/h2kill && cd /tmp/h2kill
env CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout --kill-after=5 1800 \
  <cratonvm> -XX:+UseG1GC --java-home <jdk25> --Xmx 1g -c "$CP" \
  org.h2.test.store.TestKillProcessWhileWriting
```

Both `target/classes` and `target/test-classes` are required and are NOT in
`craton-testcp.txt` — that file is the Maven dependency classpath only
(`mvn dependency:build-classpath`, line 153), and the runner prepends the two
class dirs at line 160. Omitting them fails in under a second with
`class not found: org.h2`, which in a results table is indistinguishable from a
fast failure of the test.

### Why the cap is not simply raised in the runner

`apps/` is `.gitignore`d (line 12), and `git ls-files` does not know
`run-h2-suite.sh`. The runner is **untracked**: a per-class override added there
is local to one checkout and disappears for everyone else, which is how the
fixture losses recorded in `vm/tests/common/mod.rs`'s `require_fixture` doc
happened. The durable fix is either to track the runner or to carry the cap in
the invocation, and until one of those happens this note is the record.

### Provenance of the 2026-09-06 re-verification

Both fixes are still on dev (`f1bdfd028`, the finalizable-object humongous
reclaim gate, and `607a16ea2`, the heap-full auto-box) — confirmed ancestors of
`b7ca9affa`, so nothing was reverted.

The binary was **not** built for this: the box was at 98% disk with three lanes
building, and a release build would have filled it. `a044e1fe1` was already
built in another worktree and was checked for the two properties that make it
usable — it is an ANCESTOR of dev, and both fix commits are ancestors of IT —
before any run. A binary from a diverged branch (`978d8e343`, also present and
also freshly built) was rejected for failing the first test.

The default-collector arm is the control, and it earns its place: it says the
class and the harness work at all on this binary, so a G1 PASS is a statement
about G1 rather than about the fixture. The first attempt at this table had NO
control value at all — all four arms exited in `secs=0` on a classpath error of
mine — and four rows reading `rc=1` looked exactly like four real failures.

## Why it is a different defect, measured rather than assumed

The parent page's whole subject is `zgc: arena allocation failed` —
fragmentation of the ZGC arena, reported by that collector's own guard. **This
arm produces no `arena allocation failed` line in either era**, before or after
that work, because G1 does not use the arena at all.

The numbers, from the parent page:

| era | outcome |
|---|---|
| when the parent page first measured it | 2 `OutOfMemoryError`, 31–43 s |
| on the fixed binary | 13 `OutOfMemoryError`, 1500 s cap |

**The face varies between runs**, so reproduce it several times before believing
any single one — a one-run reading here has already misled once (the parent page
had to run a five-arm interleaved A/B, `dev` binary against fixed binary, to
establish that the 2026-08-24 relocation work was not responsible; every column
came back identical).

## What to check first

G1 is an evacuating collector, so an `OutOfMemoryError` there is a
*to-space exhaustion or humongous-allocation* story, not a free-list
fragmentation one. The instruments are G1's own: the collection-set selection,
the humongous path (`is_humongous`, `pin_region_for_addr`) and whether a
completed concurrent mark cycle is reclaiming dead Old/humongous spans —
`last_ditch_reclaim` exists precisely because it is only a *finished* cycle's
cleanup that frees them.

Rerun it at least three times before drawing any conclusion from a count.

## 2026-08-29 (later): reproduced, and the census says it is NOT to-space exhaustion

Azure Linux, `--Xmx 1g`, 900 s cap, `CRATONVM_GC_STATS=1`, one binary
(`dev@a94842f04`), four G1 reps plus the default-collector control. **The "face
varies between runs" line can now be replaced with a rule**: there are exactly
two faces, they are mutually exclusive, and each has its own signature.

| rep | arm | rc | secs | real OOM | `[g1][SECURITY V7b]` dangling refs | implausible-header cycles |
|---|---|---:|---:|---:|---:|---:|
| 1 | `-XX:+UseG1GC` | 124 (cap) | 900 | 0 | **48 617** | 0 |
| 2 | `-XX:+UseG1GC` | **1 (OOM)** | 120 | **4** | 129 | **14** |
| 3 | `-XX:+UseG1GC` | 124 (cap) | 900 | 0 | **50 747** | 0 |
| 4 | `-XX:+UseG1GC` | **1 (OOM)** | 749 | **4** | 275 | **21** |
| 5 | **default collector** | **0 (PASS)** | 441 | **0** | **0** | **0** |

**The OOM face and the implausible-header cycles occur together, 2 for 2, and
never alongside the 50 000-dangling-reference face; the cap face is the exact
complement.** The default collector shows zero of all three and passes — which
is the control this page asserts and had not measured on this tip.

So G1 corrupts this workload on every run. Whether that surfaces as an
`OutOfMemoryError` or as a 900 s livelock is decided by *which* corruption the
collector notices first, not by whether corruption happened.

### The OOM face

```text
Caused by: java/lang/OutOfMemoryError: Java heap space (ByteBuffer.allocate 1048576)
    at org/h2/mvstore/FileStore.getWriteBuffer / WriteBuffer.<init>
    ... MVStore.commit -> storeNow -> serializeAndStore
```

A **1 MB** request failing on a **1 GB** heap. The collector census from the
same run says why, and it is not fragmentation and not to-space exhaustion:

```text
g1 concurrent mark: skipping gray entry 0x20080000008 (region 988) with implausible
    header — cleanup will retain all regions this cycle
g1 cleanup: implausible gray entry seen during marking — retaining all regions
    this cycle (no in-place frees)
[GC] g1 cycle #62293: kind=young cset_young=0 cset_old=0 pinned_out=5 rset_sources=0
     degraded=jit-pinned-regions-excluded,empty-collection-set
[GC] g1 humongous-eager: spans=0 bytes=0 declined_pauses=16103
```

The chain is: **a corrupt header reaches the marker → the `G1MARK-8` fail-safe
retains every region for that cycle (by design: an incomplete closure makes a
0-live verdict untrustworthy) → the humongous eager reclaim declines 16 103
pauses and frees `spans=0 bytes=0` → collection sets go empty
(`jit-pinned-regions-excluded`) → 62 293 cycles reclaim nothing → the 1 MB
humongous request cannot be served.**

So the heap is not exhausted by live data. **The collector is deliberately
declining to free anything**, because it does not trust what it marked. The
page's "check first" list should start at the corruption, not at the
collection-set selection: `last_ditch_reclaim` and the humongous path are
downstream of a marker that has already given up.

### The other face is a use-after-free the guard names outright

The capped runs never OOM'd; they logged **48 617** and **50 747** of these:

```text
[g1][SECURITY V7b] post-evacuation dangling reference: object 0x20045241910 in
    surviving region 46 still points at 0x…, which lies in a freed CSet region
    with no forwarding entry (incomplete remembered set => UAF)
```

48 300 of the 48 617 come from **one object** in region 46; the rest are spread
over seven other regions. `report_dangling_cset_ref` is a hard abort in debug
builds and a loud log in release — so a normal run has the same UAF and prints
nothing. This same signature is what `G1-9` was: the parallel evacuator not
scanning a COMPACT object's reference fields, reachable only from a full VM run.

`CRATONVM_G1_PARALLEL_EVAC=0` was therefore the obvious bisect. **It is
refuted, and it fails in the direction that rules the hypothesis out rather
than merely failing to confirm it:**

| arm | rep | rc | secs | `V7b` dangling refs | real OOM |
|---|---|---:|---:|---:|---:|
| parallel evac ON (default) | 1-4 | 124/1/124/1 | 900/120/900/749 | 48 617 / 129 / 50 747 / 275 | 0/4/0/4 |
| `CRATONVM_G1_PARALLEL_EVAC=0` | 1 | **139 (SIGSEGV)** | 652 | **95 332** | 0 |
| `CRATONVM_G1_PARALLEL_EVAC=0` | 2 | 1 (OOM) | 508 | 23 276 | 4 |

**2/2 still corrupt with the parallel evacuator off**, one of them with a hard
segfault — the third face this family is known for. The dangling-reference
count does not drop into the noise; it stays in the same 10⁴-10⁵ band the
default arm produces (95 332 and 23 276 against 48 617 and 50 747), so the
counts are not a discriminator in either direction and only the pass/fail is.

So the incomplete remembered set is not the parallel evacuator's
compact-object stride: it is in the path both evacuators share. The guard's
own wording is the thing to take literally — *"incomplete remembered set"* —
and the RSet is built before either evacuator runs.

That also retires the `G1-9` resemblance. The signature matches; the cause
does not.

### A measurement trap this page's own numbers may be sitting on

`grep -c OutOfMemoryError` over a G1 run's **stderr** is contaminated. The
empty-JIT-publication warning contains the words *"trades the wrong answer for
an OutOfMemoryError"*, so a throttled warning that fired 14 times adds 14 to
that count. Rep 1 above scores `oom=14` by that grep and **zero** by the actual
exception. The inherited figures on this page — "2 `OutOfMemoryError`" and "13
`OutOfMemoryError`" — were produced by the parent page's tooling and should be
re-derived before being compared against anything: count
`^Exception in thread` / `java.lang.OutOfMemoryError` in **stdout**, or match
`OutOfMemoryError:` with its colon.

`emptypub` is worth its own note: the warning is throttled to the first eight
then powers of two, so 14 printed lines means **at least 512** pauses ran
against a JIT root publication the conservative scan could not fill.

## Reproducing

```bash
source /data/toolchain/env.sh
cd /data/cratonvm/apps/h2database/h2
CP="target/classes:target/test-classes:$(cat craton-testcp.txt)"
<cratonvm-bin> --java-home /data/toolchain/jdk-25 --Xmx 1g -XX:+UseG1GC \
    -c "$CP" org.h2.test.store.TestKillProcessWhileWriting
```

## Related

- `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`
  — the page this was split out of, and the A/B that separated the two.
