# The work that is common to all three collectors — 2026-09-05, closed 2026-09-06

**RETIRED.** Every residual this page carried is discharged below by name: five
closed by a measurement that turned a hypothesis into a decision, two closed by
finding the answer already in the tree, one closed by a defect this page's own
follow-up list found. Two of its three defaults are reverted, and the reason in
both cases is the evidence rather than the mechanism. What it discovered and
could not close — a compiled frame that keeps a young reference the collector
moved — moved to its own record, with a ten-second reproducer this page did not
have.

A review of `gc/` asking only one question: **what is shared by Generational,
G1 and ZGC, and what is wrong with it?** Eight findings, what landed against
each, and what the follow-up measured.

The recurring shape is worth stating up front, because it predicts most of the
list: **ZGC's modules are where the measurements happened, and almost nothing
learned there was carried back.** Three of the eight findings are literally
"the good implementation exists, inside `#[cfg(feature = "zgc")]`".

---

## The follow-up, 2026-09-06 — what every open item turned out to be

Every row below was an open item on this page on 2026-09-05. None of them is
one now. The measurements are on Azure host 2, release binaries, and every
throughput row uses **concurrent paired arms** rather than a sequential ABBA —
see "How these were measured" at the end, which is the methodological correction
that also invalidates two of this page's own numbers.

| # | the residual as this page left it | what closed it |
|---|---|---|
| 1 | the collector-side slot half: a relocating collector storing through the slot | **priced.** The fix-up it would delete costs 0.2–9.8 ms per collection, scaling with the relocation set. Real, but it is an ABI change across ~35 remap companions for a few percent of a large pause. |
| 2a | card-marking statics | unchanged: a missed write is a missed root. Not attempted, and now not needed — 2c prices the walk it would accelerate. |
| 2b | narrow the statics scan by declared type | **refuted by the size of the prize.** The scan is 10–432 µs per collection, and on the biggest static surface measured **72% of the slots already hold an object**, so a declared-type filter could skip at most 28% of it. |
| 2c | deduplication / feeding the snapshot into the scan | unchanged; both were already decided here with reasons. |
| 5 | give G1 the exact object-start bitmap | **refuted.** G1's predicate is **0.24% of a run** (289,693 calls, 12 ms of 4.9 s) and **99.98% of calls already accept**, so an ACCEPT-only bitmap has almost nothing to short-circuit. |
| 6a | re-run the +153% four-worker parallel MARKING figure | **confirmed in direction, and it is now small.** The lever engages (`workers_last` 1→4→8, `parallel` 0→23); the drain goes 72 → 76 → 80 ms, monotone. The lock removal took the rest. |
| 6b | one shared work-stealing GC worker pool | **the throughput case is refuted by 6a**: adding workers to the pool that exists makes it slower, so consolidating pools is a maintenance argument, not a pause one. |
| 7 | old-generation give-back | **blocked, on a named defect with a reproducer** — see the compiled-frame record. The young give-back's fault window is real and reachable; adding a second one to the old generation before that is fixed would be adding a second way to crash. |
| 8a | lock-free `needs_gc` | **landed**, with the choke point the page asked for and an oracle: **63,661 checks, 0 divergences** on multi-threaded H2. Measured at no difference on 8 threads — it is a shape change, not a throughput claim. |
| 8b | unify the three remembered sets | **already answered in the tree.** `g1_cards.rs` reassessed it on 2026-09-05 — the stated blocker is indeed gone, and the merge is still not worth making for a different and better reason (thirty shared lines, two genuinely different structures around them). |
| 8c | ~3,000 lines of unwired machinery | **closed as a scope call, once.** The answer now sits in `gc/src/lib.rs` beside the module declarations, so the next reader meets it where they meet the modules instead of re-deriving it in a fourth file. |
| 8d | volatile striping flat past 4 threads, "cause still unexplained" | **not open — it was fixed on 2026-08-27.** This page read the historical half of `collector.rs`'s own comment as a live finding. The cause was the stripe pool fitting in one cache line; padding each stripe to 128 bytes measured **1.64x at 24 threads** with a 1.00x single-thread control. |
| f/u 3 | `CRATONVM_DBG_STATIC_SLOT_VERIFY=1` over a real application | **done, and the verifier was corrupting the VM.** See below. |
| soak | growing young arenas, multi-threaded allocator | **done**, and it took a default with it. |

---

## The verifier this page told you to run was double-remapping live statics

`CRATONVM_DBG_STATIC_SLOT_VERIFY=1` over H2 (`DodH2JdbcSuite`) and Spring Boot
with an embedded Tomcat over TLS, generational collector:

```text
h2jdbc  covered=1440 missed=0 chained=0    moved=28044
h2jdbc  covered=1692 missed=0 chained=547  moved=702
h2jdbc  covered=1692 missed=0 chained=0    moved=61816
tcssl   covered=4140 missed=0 chained=0    moved=188939
tcssl   covered=4729 missed=0 chained=1298 moved=6464
tcssl   covered=6621 missed=0 chained=2259 moved=112136
```

**`missed=0` on every collection of both applications**, against pointer maps up
to 206,002 entries and static surfaces up to 6,621 slots. That is the answer
this page asked for: the recorded slot list is complete on real applications,
not only on the 178-slot synthetic it had.

`chained` is the column that was not there before, and it is why the answer took
two runs. The shipped verifier had a single `missed` count and a premise stated
in its own comment: *"anything the slot walk covered is already remapped, so a
second `update_value_ref` on it is a no-op (the new address is not itself a
key)"*. That premise holds for one Cheney semi-space pair and **fails for a
composed map**. `collect_garbage_inner` chains the young map's values through
`compact_map` and then merges `compact_map` whole, and a sliding compaction
routinely moves object X down onto the address object Y just vacated — so a
value in the map is a key in the map, for a different object.

The verifier therefore saw its own correctly-patched slots "still resolving",
counted them as misses, and **applied the map to them a second time**, sending
each reference to wherever the *previous tenant* of that address went. Up to
**2,259 live static slots in a single collection**. Diagnostic-mode only — but
every earlier reading taken through this verifier was taken on a VM it was
corrupting, which includes this page's own `missed=0 moved=12235` line.

Split into `missed` (the real defect: a slot the scan never recorded) and
`chained` (a recorded slot whose new address is another object's old one), and
the second application removed.

---

## The eight findings, as filed on 2026-09-05

Kept verbatim below the correction line. Read the closure table above
first: several paragraphs here state an open item that is no longer one,
and two state a measurement the follow-up refuted.

## 1. Roots are values, not slots

`GarbageCollector::collect_garbage` takes `roots: &mut [ObjectRef]` — object
*addresses*, not the addresses of the slots holding them
(`gc/src/collector.rs:597`). A moving collector therefore cannot fix a root in
place, and everything downstream follows:

* every relocating cycle builds a `PointerMap` with one entry per moved object,
  and pays a hash probe per reference, inside the pause;
* the VM re-walks the whole root surface afterwards to patch through it —
  `update_all_roots` plus roughly **35** hand-written `gc_update_*` /
  `remap_*` companions. `pointer_map` appears in **574** places across `vm/`
  and the native crates;
* forgetting one is a silent use-after-free. The ~110-line comment block at the
  end of `collect_roots` is a list of the subsystems where that already
  happened (`ClassValue` cache, JBoss MSC, LogManager, annotation proxies,
  `InetAddress`, `DatagramSocket`, NIO `SelectionKey`, FFM upcalls, TLS
  key/trust managers, ForkJoin results…).

**Landed:** the first slot-carrying root path in the VM, for statics.
`StaticsBlock` is a leaked `Box<[Value]>` whose base is "stable for the life of
the VM", and `grow_to` deliberately leaks the old block so a lock-free reader's
pointer stays valid — so a static slot address survives both a collection and a
rehash of the owning map. `collect_roots` records those addresses;
`update_all_roots` walks them instead of re-sweeping every slot of every class
under the `statics` **write** lock.

Take-once semantics make a stale list impossible rather than merely unlikely: an
`update_all_roots` not preceded by a scan on the same thread gets `None` and
takes the old full-walk path. Kill switch `CRATONVM_GC_STATIC_ROOT_SLOTS=0`;
coverage verifier `CRATONVM_DBG_STATIC_SLOT_VERIFY=1` re-walks the slow way and
reports any slot the recorded list missed (the same shape
`CRATONVM_DBG_ROOTSNAP_VERIFY` uses for the frozen-frame cache).

The verifier prints the SHAPE, not just its failures — covered, missed, and the
size of the collection's pointer map. A silent run on this path has two very
different explanations and the likelier one is not "covered everything": it is
that `take_static_ref_slots` returned `None`, the fast arm was skipped, and the
verifier never ran. That is the difference between a coverage oracle and a line
that has never fired.

**Measured**, on `probes`-style churn (400k allocations, live statics, 96 MB
heap), generational backend:

```text
[static-slot-verify] covered=178 missed=0 moved=12235
[static-slot-verify] covered=178 missed=0 moved=12887
[cratonvm] static root slots: patched=356 full-walk-fallbacks=0
[cratonvm] static root slots: patched=178 full-walk-fallbacks=0   (ZGC)
```

`missed=0` against a pointer map of twelve thousand moved objects is the
coverage evidence. `fallbacks=0` is the one that matters more: it says the
scan/fix-up pairing held on *every* collection rather than silently degrading to
the old full walk on some of them, which is the failure mode the take-once
design is guarding against and the one a coverage number alone would not show.

This is a first reading on one shape of workload, not a soak. The follow-up list
at the end of this page still asks for it over H2 or Tomcat before anyone builds
further on the slot path.

**Not landed:** the collector-side half — a relocating collector storing the new
address *through* the slot, retiring the `PointerMap` for that root class
entirely. That is an ABI change to `collect_garbage` and belongs with a
collector change, not a VM one. Statics are the proof that the shape works.

**Note for whoever takes it further:** this is also what unblocks finding 2's
statics work. A cache of "which static slots hold references" is invalid across
a moving collection *only because roots are values*; with slots, the addresses
are stable and the cache survives.

## 2. The root scan is serial, allocating, and re-scans immutable state

`collect_roots` (`vm/src/memory/roots.rs:238`) is ~41 sections that started at
`Vec::new()`, never deduplicate, walk **every static field of every loaded
class** on every collection including young ones, and run entirely on the
initiating thread. `update_root_snapshot` does a second frame walk immediately
before it and `update_all_roots` a third after, so a collection walks the root
surface three times.

**Landed:** the root vector is pre-sized from a monotone high-water hint. A real
application's root set is tens to hundreds of thousands of entries appended
across those 41 sections, and `Vec`'s doubling made that a dozen-plus
reallocations — each a fresh allocation plus a memcpy of everything gathered so
far — inside the pause, before any marking starts. Nothing reads the hint for
correctness, so a stale or torn value cannot do worse than the `Vec::new()` it
replaces. The post-collection statics walk is also gone (finding 1).

**Not landed, and why:**

* **Card-marking statics.** Sound only if *every* write to a static reference
  goes through the marked path, including JIT inline static stores and native
  writes. A missed write is a missed root, i.e. a UAF. Not attempted blind.
* **Narrowing the statics scan by declared type.** Correct per the JVM spec —
  an `int`-declared slot cannot hold an object — but this tree has a documented
  history of lost-tag values and long-smuggled `jobject`s, and the scan's
  tolerance for them is load-bearing in places. Would need the verifier from
  finding 1 run over a real workload first.
* **Deduplication.** Deliberately not done: sorting a large root vector is
  `O(n log n)` inside the pause, and a duplicate root is rejected by the mark
  bit almost immediately. The cost is not where it looks.
* **Feeding the snapshot into the scan.** The two are not the same scan —
  `update_root_snapshot` uses `scan_frame_roots`, `collect_roots` uses
  `scan_local_objects` plus the conservative variants. Merging them changes what
  is rooted.

## 3. Heap geometry was published into overloaded, collector-specific tables

`gen_heap.rs` carries four process-global six-word geometry tables, and their own
comments tell the story:

| table | question | who fills it |
|---|---|---|
| `JIT_REGION_BOUNDS` | "is mapped" **and** "may an inline store skip the barrier" | Generational only — G1/ZGC **must** leave it empty (defect G1-2) |
| `JIT_READ_BOUNDS` | "is mapped, for loads" | Gen, G1, ZGC |
| `MOVABLE_BOUNDS` | "may an object here move" | Gen, G1, ZGC |
| G1's barrier table | "the numbers an inline barrier needs" | G1 |

`JIT_READ_BOUNDS` exists because of the first row: *"One table, two questions,
opposite answers... Hence the sibling."* G1's table opens *"This is the same
lesson a third time."*

Every one of those is a question about a **capability**. None is "where is the
heap" — and that is why asking it kept going wrong.
`compressed_oops::enable_for_live_heap` needs a base and a limit and read
`JIT_REGION_BOUNDS`, the one table G1 may never fill, so it answered *"no live
heap regions published (non-generational backend?)"* for G1 and ZGC. Compressed
oops was refused on the **default** collector for a reason that has nothing to
do with compressed oops.

**Landed:** `gc::heap_geometry` — one table that asks only where the heap is,
carries no permission of any kind, and is filled by all three backends (the
generational heap's three arenas, G1's *reservation* rather than its committed
prefix, ZGC's arena envelope unconditionally). `enable_for_live_heap` derives the
narrow-oop window from it.

The audit gate stays and moves into the open: `vm_init` now says the backend has
not been audited for 4-byte reference slots, rather than implying the geometry is
missing. Two gates that were one by accident are now two, and the second can be
moved one backend at a time.

Checked against the built VM rather than asserted. `CRATONVM_COMPRESSED_OOPS=1`
with `-XX:+UseGenerationalGC` still reports *"compressed oops ON: HeapBased
base=0x1ae75591000 shift=3 … 386 class layouts re-laid out"* — the check that
mattered, since that path now derives its window from `heap_geometry` and from
nothing else, so a backend that failed to publish would have turned a working
configuration OFF. On the default collector it reports the audit gate by name.

Guarded by a source witness that all three backends call `publish_heap_span`,
because the defect this module exists to undo is *a call that does not happen* —
nothing was broken when `enable_for_live_heap` answered "no live heap regions
published" for G1 and ZGC; nobody had written the publish. No runtime assertion
can see that, and a behavioural test cannot be written here either: the table is
process-global and this crate's tests share one process. (The tests added with
the module raced each other on that very property and had to be serialised — the
first run after the witness landed caught it.)

**Correction to the original review:** `VmHeap::conservative_addr_span`'s ZGC arm
was cited as still returning `None`. It was fixed on 2026-08-07 and now delegates
to `ZgcRealHeap::conservative_addr_span`. The stale text is the historical half
of that arm's own comment.

## 4. Four object-start / mark bitmaps, of which one is good

| | atomic | alignment-exact | `remove` | backward interior scan |
|---|---|---|---|---|
| `young_mark::ObjectStartBits` | no (`&mut`) | yes | no | no |
| `young_mark::YoungMarkBits` | yes | **no** | no | no |
| `mark_bitmap::MarkBitmap` (G1 + Gen) | yes | **no** | no | no |
| `zgc::starts::ZObjectStartBits` | yes | yes | yes | yes |

`ZObjectStartBits`'s own header enumerates the other three and why it could not
reuse them, concludes *"Neither file is edited"*, and writes a fourth. The reason
it had to was not a property of the type — it references no `zgc` item at all —
but that it lived inside a `#[cfg(feature = "zgc")]` module.

**Landed, three things:**

1. **`MarkBitmap::clear` was an atomic RMW per word** (`swap(0, AcqRel)`). Over a
   4 GiB old generation that is 8.4M locked read-modify-writes inside the
   mark-start pause. The identical structure has the measurement:
   `ZObjectStartBits::clear_all` records the sibling operation it replaced at
   **94–96 % of the mark-start pause** (34 ms of 35 on a 4.6M-entry registry, 66
   of 69 on a 10.8M one), and settled on relaxed stores plus one trailing
   `Release` fence. Every `MarkBitmap::clear` caller is stop-the-world, so a
   marker synchronises with the pause exit and never with an individual word —
   the old doc conceded this by keeping a trailing `SeqCst` fence to cover what
   per-word `AcqRel` could not.
2. **`clear` had no way to know it had nothing to do.** G1 calls it from
   `G1Region::reset()` for every region a cleanup frees, and a young pause marks
   into almost none of them. `any_marked` is set *before* the `fetch_or` — that
   order, not the reverse, is what makes a `false` safe to act on.
3. **`new` materialised the bitmap one `AtomicU64` at a time**, and both
   accessors ran a `checked_add`, two range compares and a redundant length test
   per bit on the innermost loop of a mark cycle. Now one `alloc_zeroed`-backed
   allocation and one shared `locate()`.

**Also landed:** `ZObjectStartBits` promoted verbatim to
`gc::heap_bitmap::HeapBitmap`, with `zgc::starts` re-exporting it under the old
name so that collector is untouched. Its two A/B switches stayed behind — they
are levers over ZGC's *use* of the bitmap, not properties of the bitmap. No
behaviour change; the point is that the other two collectors can now reach the
good implementation.

## 5. `is_object_address` is a header-shaped guess in all three

`gen_heap.rs:4286` is ~180 lines of "does this look like a header": alignment, a
linear scan of three bounds pairs, a commit-granule probe, two enum-tag
validations, reserved-field plausibility, a slot-count cap, an extent
computation, a **second** scan of the bounds, and a second commit probe. G1's is
a sibling copy; ZGC's is a bitmap probe.

It is still only a guess, and the comment at `gen_heap.rs:4405` documents the
false-positive class it cannot eliminate: an interior 16-byte `Value` cell of an
`Object[]` decodes as a self-consistent fake header (`class_id=4`,
`num_slots=4`), caught only afterwards by the extent check.

**Landed:** the duplicate bounds scan is gone. The extent check re-scanned all
three bounds pairs — six more `Acquire` loads — to find the region the
containment loop had already identified. It now asks that region directly, with
the full scan kept as a fallback so the answer stays bit-identical without
resting on the arenas being disjoint. This is the VM's hottest validator: every
conservative stack word, every ambiguous operand slot and every JIT helper probe
goes through it.

**Also landed:** the actual fix. `CRATONVM_GC_OBJECT_STARTS` (opt-in) maintains
an exact object-start bitmap per arena — a bit per 8 bytes, set in `hand_out`,
the single door every allocation funnels through, cleared in `add_free_block`,
the single door every free does, and cleared wholesale on all three reset paths.
The removal half is not optional: without it a larger object later allocated over
a freed base would see the stale bit, be accepted at an interior address, and
take a mark-bit write into the middle of a live object — the corruption the
extent check exists to stop.

```text
[cratonvm] exact object-start answers: hits=1112587 misses=799889
```

58% of calls answered without the extent arithmetic or the second commit probe;
nothing at all with the switch off.

**Two things the first version got wrong**, both caught by building the
instrument before trusting the change, and both worth recording because they are
the traps this shape has:

* It consulted the bitmap **before** the header checks. `hand_out` is the door
  for every ALLOCATION, not every OBJECT: a TLAB chunk is one hand-out and its
  base carries the bit whether or not an object was ever bump-allocated at it.
  Accepting on the bit alone hands the caller an `ObjectRef` to a chunk whose
  first bytes are not an `ObjectHeader` — the exact corruption the function
  exists to prevent, reintroduced by the thing meant to make it exact. The fast
  path now sits after the kind, element-type and reserved-field tests, which are
  cheap byte loads; what it skips is the expensive half. The hit count is
  unchanged by the move, so nothing was being accepted that those tests would
  have rejected.
* It reached the bitmap by **locking the arena**. This function was made
  lock-free on purpose — its own comment records that the triple-mutex
  containment check it replaced "contended catastrophically" with the allocator
  — and a `try_lock` is still an atomic read-modify-write on a line every
  allocating thread wants. The bitmaps are now published into a lock-free
  pointer slot beside `commit_bits`, with the same retire-before-drop ordering.

**ACCEPT ONLY**, and that asymmetry is the safety argument. A miss falls through
to the deduction unchanged. The bitmap is knowably incomplete — objects
bump-allocated inside a TLAB chunk never reach `hand_out` — so using it to
REJECT would drop live roots. Used to accept, incompleteness costs only the path
that was already there.

Off by default: a bit per 8 bytes is capacity/64 of side table, 64 MB for a 4 GB
heap. **Still owed:** G1, which is NOT the one-line change it looks like — it does not
allocate through `Arena` at all (its backing is a `ReservedHeap` carved into
`RegionBuf` slices, and `alloc_in_region` never reaches `hand_out`), so it has no
bitmap to consult; and the measurement that would justify a default. The miss half of the census cannot
yet separate a genuine non-object from a real object the bitmap never saw
because a TLAB bump-allocated it; that needs a workload, not another counter.

## 6. Parallel marking is currently a regression, and the cause is shared

`external_roots.rs` records `roots_for_owner` at **20–29 % of mark samples** on
both the serial and one-worker arms, and **four workers at +153 % pause against
zero**, monotonic in worker count — a lock, not a start-up offset.
`PROVIDER_COUNT` had removed the cost for the *empty* registry (every
`--jdk-only` run, every unit test) and left it for the non-empty one, which is
every real application run, because `native-collections` registers its provider
at VM startup.

**Landed:** registration republishes an immutable table (`Box::leak`, one pointer
carrying its own length) and readers take one `Acquire` load. No lock, no
reference count — a reference count would be the shared-line write this exists to
delete. Registration is monotone and has three call sites in the whole tree, so
the superseded tables are a bounded few hundred bytes.
`with_external_roots_for_owner` hands the caller a slice borrowed from a
per-thread scratch buffer that lives for the whole cycle, so a marker walking ten
million objects allocates at most once between them; both per-object call sites
(gen's old-gen BFS, `ZgcRealHeap::visit_overlay_edges`) moved to it.

**Not landed:** one shared GC worker pool. There are three — `EvacPool` (G1
evacuation, persistent, and its own doc says *"It is not a general executor…
no work stealing between jobs"*), `ZMarkCoordinator` (ZGC, persistent +
work-stealing, the most capable), and raw `thread::spawn` in `concurrent_mark`.
Consolidating on the stealing pool would let root scanning, marking, evacuation,
sweeping and bitmap clearing all parallelise without a fourth concurrency design
to audit. Large, and worth doing after finding 6's lock removal has been
re-measured — the +153 % number should be re-run first, because it may no longer
be true.

## 7. Memory returned to the OS — the original claim was wrong, and there is a real bug underneath

**Correction:** the review said "only ZGC returns memory to the OS". G1 has a
complete, correctly-ordered heap-shrink path —
`uncommit_trailing_free_regions_within`, opt-in behind `CRATONVM_G1_UNCOMMIT`.
The accurate statement was: **ZGC does it by default, G1 can on request, and the
Generational collector could not at all** — no `decommit`/`uncommit` call
anywhere in `gen_heap.rs`. It can now; see below.

**Found and landed instead — a fault, not a slowdown.** `ZgcRealHeap::
with_capacity` publishes `[arena_base, arena_end)` into `JIT_READ_BOUNDS` slot 0,
and the JIT's guarded `getfield` emits a raw load of the receiver's class-id word
for any address inside that range. The give-back at the top of every collection
then decommits whole granules **in the middle of that same range** — and
`platform_decommit` is `madvise(MADV_DONTNEED)` + `mprotect(PROT_NONE)` on Unix
and `VirtualFree(MEM_DECOMMIT)` on Windows, so a released granule **faults on
touch**; it does not read back as zero. `Arena::decommit_free_blocks` says so
itself, and records that believing otherwise is how two slides came to write into
released granules.

Nothing re-narrowed the bound. G1 gets the same pair right and says why —
publish the narrower bound *before* unmapping — but G1 only shrinks a **prefix**,
so it has a narrower bound to publish. ZGC's holes are interior, so the only
sound move is to withdraw the range. That is now a one-way latch: one relaxed
load per collection that released anything, fires at most once per process, and
afterwards every guarded site falls through to the checked helper — which is what
an unpublished collector already gets, and the fail-safe direction. Pinned by a
source witness, because `JIT_READ_BOUNDS` is process-global and this crate's
tests share one process.

**Also landed:** generational uncommit. `CRATONVM_GEN_UNCOMMIT` (opt-in) hands
the EVACUATED young semi-space back at the end of each young collection. That
arena is the one the collection evacuated FROM, so nothing live is in it by
construction, and the very next thing the cycle does to it today is zero it —
this is the same statement made to the OS instead of to the bytes.

```text
[cratonvm] generational young uncommit: 31457280 bytes returned to the OS
```

30 MiB of a 96 MB heap, against no line at all with the switch off.

**The ordering, which the first attempt got wrong in both available ways.**
Putting the give-back in an `else` of the deferred-wipe branch made it
STRUCTURALLY DEAD — `deferred_wipe` is `Some` on the default path — so the arm
never ran and the census read zero for a switch that was on. It was caught only
because the census was there to read. Running both against overlapping spans is
worse than dead: a released granule faults on write, and the wipe thread would be
writing into memory the give-back had returned. And they are not interchangeable,
so the wipe is not simply skipped — a released granule comes back from the OS
zeroed, but `Arena::hand_out` does not zero and the give-back only releases WHOLE
granules rounded inward, so the partial granules at a span's edges still hold the
previous cycle's object bytes. Give back first, then wipe the remainder;
`Arena::retain_committed_spans` is that remainder.

Off by default for the reason `g1_uncommit` gives about itself: this collector
publishes its young arenas' full reserved range into `JIT_REGION_BOUNDS` and
`JIT_READ_BOUNDS`, and a decommitted granule faults on touch. That window is not
created here — the young arenas already commit lazily while the published bound
covers the whole reservation — but it is WIDENED, from "granules never yet
allocated into" to "granules that held objects one collection ago".

**Still owed:** the old generation, which cannot be done at a call site. It is a
`Vec<u8>`, committed in full at construction, with no reservation to shrink;
giving it lazy commit is a change to its allocator (33 `self.data` sites plus
commit-on-demand plumbing), not a wiring change.

## 8. Smaller shared items

**Landed.** `GenerationalHeap::young_gc_threshold` was a `Mutex<usize>` read by
`needs_gc` on the **allocation** path — a shared cache line taken exclusive by
every allocating thread to read one word that only changes at a safepoint. Both
writers run stop-the-world, so load/store is exactly equivalent to what the lock
provided. It is now an `AtomicUsize`.

**Deferred, with the reason.** `needs_gc_with_jit_allocation_frame` still opens
with `self.young_from.lock()`, because it needs `used()`, `free_list_bytes()` and
`capacity()`. Publishing those lock-free means instrumenting **26** mutation
sites in `arena.rs` (`self.cursor`, `self.high_cursor`, `self.free_bytes_total`),
and a missed site silently mistunes the GC trigger — degrading to the
allocation-failure backstop rather than corrupting anything, but potentially by a
lot. The right shape is a single `republish` choke point plus a debug verifier
that re-reads the locked values and asserts agreement, and the right order is to
measure the lock's actual contribution first. ZGC's equivalent predicate is two
relaxed loads, so the target is known.

**Reported only:**

* **Three remembered-set implementations** — `card_table.rs` (Gen),
  `g1_cards.rs` (G1), `zgc/remembered.rs` (ZGC). `g1_cards.rs`'s doc explains
  why it could not reuse the generational one: its dirty path was a per-thread
  `Vec` behind a `Mutex`, drained at a safepoint. **That reason is gone** —
  `CardTable::mark_dirty_lockfree` is now a bounds check, a shift, a relaxed load
  and a conditional release byte-store, byte-for-byte what the JIT's inline
  barrier emits. The stated blocker to sharing no longer exists.
* **~3,000 lines of unwired machinery** — `metaspace.rs` (1,623) and
  `class_unloading.rs` (1,420) have no caller anywhere in the workspace. Both are
  honestly labelled and deliberately kept, so this is a scope call rather than a
  defect; noted because three collectors' readers keep tripping over it (there is
  a 20-line comment in `g1.rs` existing solely to answer the grep).
* **Volatile-field striping scales flat past 4 threads** even after the
  false-sharing fix (`collector.rs:38`). The comment records the measurement and
  states the cause is still unexplained. An open question, not a suggestion.

---

## Verification, and one `rc=124` that was not a regression

`bash regression-suite/run.sh` on the merged tree, debug binary, `TIMEOUT=600`:
**90 passed, 0 failed.** A confirming run on the exact shipped binary (which
added two relaxed counters and a flattened diagnostic string) came back **89
passed, 1 failed — `RMapGcStress`, `rc=124`**, which is the harness's own
timeout and which it labels `HARNESS FAULT — TIMED OUT; the harness killed the
VM, it did not fail`.

That label is a hint, not an attribution, and this vector had now timed out in
two runs of three. So it was attributed rather than assumed, with the kill
switch the change shipped with — one binary, both arms, interleaved so a fixed
order could not manufacture a difference:

| run | `CRATONVM_GC_STATIC_ROOT_SLOTS` | wall | result |
|---|---|---|---|
| 1 | default (on) | 718 s | PASS, 378053 checks |
| 2 | `0` | 1121 s | PASS, 378053 checks |
| 3 | default (on) | 1073 s | PASS, 378053 checks |
| 4 | `0` | 1091 s | PASS, 378053 checks |
| 5 | default (on) | 768 s | PASS, 378053 checks |
| 6 | `0` | 677 s | PASS, 378053 checks |

Two readings, and the second is the one that matters:

* **Identical check counts in every run, both arms.** The slot-carrying root
  path produces the same 378,053 assertions as the full walk on the tree's most
  GC-hostile vector. That is the correctness statement.
* **Both arms routinely exceed the 600 s budget.** The spread *within* the
  control arm alone is 677–1121 s, a factor of 1.65, and the arm with the change
  DISABLED produced the two slowest runs. A debug binary on this host simply
  does not fit this vector inside `TIMEOUT=600` reliably; the run that passed
  landed on a fast draw. There is no signal here to attribute to the change, and
  the medians (768 s on, 1091 s off) point the other way from a regression.

Isolated at `TIMEOUT=1800` on a quiet host, `RMapGcStress`, `RMapResizeGc` and
`RSyncMethodJit` all pass.

**Unrelated, and worth someone's attention:** the first timed-out run left a
`cratonvm.exe` child alive at 1.2 GB that `Stop-Process -Force` and `taskkill /F`
both refused to terminate, and that was still resident 80 minutes later. The
harness's `timeout` kills the wrapper; this child outlived it in an
uninterruptible state. It burns no CPU, so it only costs memory — but it costs
it silently, on the same host as every subsequent run, which is exactly the kind
of thing that makes a timing-sensitive vector look flaky.

Other gates: 1866/1866 `cratonvm-gc` unit tests, `cargo check --workspace
--all-targets` clean, `cargo clippy -p cratonvm-gc` clean, and the
`--no-default-features` fallback build — which `gc/Cargo.toml` records as
UNVERIFIED because no CI job compiles it — verified compiling.

## The two defaults, and the timing that justifies them

Both features shipped opt-in and were defaulted ON after the
HotSpot-differential regression suite came back **90 passed, 0 failed** with them
enabled on the generational collector. That answers correctness. It does not
answer cost, and the give-back in particular does syscalls per collection —
`madvise`/`VirtualFree` on the way out, and a commit per 2 MiB granule on the way
back in — so "it passes" and "it is free" are different claims.

Timed on the probe workload, `-XX:+UseGenerationalGC -Xmx 96m`, arms
interleaved, and **every arm checked for `rc=0` and its `CHURN_OK` line** — the
first attempt at this table set a bogus `CRATONVM_X=0` as its no-op, which this
VM refuses as an unknown token, so it may have been timing a process that never
started:

| | run 1 | run 2 | run 3 | median |
|---|---|---|---|---|
| `object-starts` ON | 2524 ms | 2650 ms | 2975 ms | **2650 ms** |
| `object-starts` OFF | 2624 ms | 3015 ms | 2867 ms | 2867 ms |
| `gen-uncommit` ON | 2483 ms | 2552 ms | 2566 ms | **2552 ms** |
| `gen-uncommit` OFF | 2504 ms | 2562 ms | 2670 ms | 2562 ms |

Neither costs measurable time. The give-back is free to within noise while
returning 30 MiB; the bitmap's median is ~7% faster with it on, though the
spreads overlap and the honest reading is "no measurable cost, possibly a small
win" rather than a speedup claim.

What these numbers do NOT cover, and a soak still should: a workload whose young
arenas GROW (the bitmap is rebuilt there, and that path is exercised by nothing
above), and a multi-threaded allocator, where the give-back's re-commit syscalls
land on the allocation path of every thread rather than one.


---

## The follow-up list as it was written, for the record

## What a follow-up should measure first

1. ~~Re-run the +153 % four-worker number~~ — **attempted, and it produced two
   results: one about the collector and one about the instrument.**

   **The instrument first, because it invalidates prior work.** G1 counted
   NOTHING about which evacuator ran — no `workers=` in `[GC-STAT]`, no census in
   `evac_pool.rs`, nothing in the dispatcher. And the nearest-looking counter
   belongs to someone else: `[GC] par_evac` is the GENERATIONAL evacuator's, in
   `gen_evac.rs`, and reads zero under G1 whatever G1 did. Read as G1's it says
   "the parallel evacuator never runs", which is striking and false — a zero from
   an instrument armed where it cannot fire.

   With `[GC] g1 young evacuation: parallel=N serial=N workers_last=N` added,
   two of the three worker-count levers turn out not to reach this path at all:

   | lever | `workers_last` |
   |---|---|
   | `CRATONVM_GC_PAR_THREADS=1` / `=16` | 23, 23 — **inert** |
   | `-XX:ParallelGCThreads=1` / `=8` | 23, 23 — **inert** |
   | `CRATONVM_G1_WORKERS=1` / `=8` | 1, 8 — engages |

   `CRATONVM_GC_PAR_THREADS` is the generational young collector's knob, and
   `gc_worker_threads_for` reads `config.gc_worker_threads` while `vm_init` sets
   `parallel_gc_threads` from `-XX:ParallelGCThreads` — a separate field. **Any
   previous G1 worker-count A/B run on either of the first two compared a binary
   against itself.**

   **The collector.** `G1ChurnPauseProbe 96 100`, `-Xmx 256m`, debug binary,
   five interleaved reps per arm, engagement printed on every run (18-19 parallel
   cycles, `workers_last` 1 vs 8):

   | | median pause | p90 pause | total pause | wall |
   |---|---|---|---|---|
   | 1 worker | 381 ms | 2201 ms | **12.7 s** | 34 s |
   | 8 workers | 389 ms | **2836 ms** | **15.6 s** | 33 s |

   Adding workers leaves the median and the wall clock unchanged and makes the
   TAIL and the total pause time worse — +23 % total, +29 % p90, in the same
   direction in 4 of 5 reps for both. So "more workers is better" is not
   supported here, and the August +153 % figure points the same way.

   **What this is not.** It measures parallel EVACUATION; the +153 % figure was
   about parallel MARKING, so this informs that question without answering it.
   Debug binary, one workload, 19 pauses per run — the medians are stable, a p90
   from 19 samples is coarse. And it does NOT retire finding 6's second half: a
   shared work-stealing pool is a different design from more workers on the
   existing one, and this says the existing one does not scale, not that nothing
   would.
2. A per-cycle **`MarkBitmap::clear` timing** on a G1 workload with many freed
   regions — the `any_marked` early return should show up as a step change in
   cleanup cost, and if it does not, the bitmap was not where the time went.
3. `CRATONVM_DBG_STATIC_SLOT_VERIFY=1` over a real application (H2 corpus,
   Tomcat) before anyone builds further on finding 1's slot path. The reading
   above (`missed=0`, `fallbacks=0`) is one synthetic workload with 178 static
   reference slots; a real application has thousands, loads classes while
   collections are in flight, and exercises the deferral branch this records
   ahead of.
4. ~~Split the miss half~~ — **done, and it answers the question by closing it.**

   `hits=1112587 misses=799889 (of which real objects: 799889)`. Every miss is a
   real object base. So the bitmap's incompleteness on this workload is entirely
   TLAB-allocated objects, and plumbing the TLAB bump would take coverage from
   58% to essentially 100%.

   **Read the denominator before reading that as 42% of all probes.** The
   bitmap check sits *after* the kind, element-type and reserved-field tests, so
   a zero or a small integer never reaches the miss counter — it is rejected
   upstream. `misses` therefore counts only candidates that already look like
   object headers, and among those "is a real object" is very nearly implied.
   The number is still the one that matters (it says the recoverable population
   is all of it) but it is not a false-positive rate for the predicate.

   **And it should still not be built.** The cost lands on the wrong path.
   `Tlab::alloc` is a pointer bump with no atomics, and `HeapBitmap::insert` is
   a `fetch_or` — so recording TLAB objects means an atomic read-modify-write
   per allocation. Worse, `cursor` and `end` are a JIT CONTRACT at byte offsets
   0 and 8, and the x64 backend bumps them **inline in emitted code**
   (`emit_inline_tlab_new_ir`); `Tlab::alloc` is not even on the compiled path.
   Recovering the coverage would mean adding a locked RMW on a shared bitmap
   word to the hottest emitted sequence in the VM, to speed up a validator that
   runs during root scanning. That is the wrong trade, and the census is what
   made it a decision instead of a guess.
5. **G1's `is_object_address`** — and note that the obvious shortcut does not
   exist. G1 does NOT allocate through `Arena`: its backing is a
   `heap_reservation::ReservedHeap` carved into `RegionBuf` slices, and
   `alloc_in_region` never reaches `Arena::hand_out`, so no bitmap is maintained
   for it. Giving it one is a real change at its own allocation chokepoint, not
   a consultation of something already there. It is also worth less: G1's
   predicate is already the cheaper of the two — no extent computation and no
   second commit probe — so the expensive half the bitmap replaces in
   `gen_heap` is not there to replace. Measure before building it.

---

## The two defaults, reverted, and the two different reasons

Both were flipped ON on 2026-09-05 on the strength of a 90/0 HotSpot-differential
regression suite plus a three-run-per-arm sequential ABBA. Both are back to
opt-in. The switches, the kill switches and the instruments all stay.

### `CRATONVM_GEN_UNCOMMIT` — it SIGSEGVs H2 in ten seconds

`-XX:+UseGenerationalGC` over `org.h2.test.jdbc.TestPreparedStatement` alone:

| arm | rc |
|---|---|
| this flag ON | **139**, fault address inside a `site=unbumped-middle` released span, fault pc in a live JIT code buffer |
| `CRATONVM_GEN_UNCOMMIT=0` | 0 |
| `CRATONVM_GC_RESERVE=0` (nothing decommits) | 0 |
| `--nojit` | 0 |
| `CRATONVM_GC_OBJECT_STARTS=0` | 139 |

Reproduced on `origin/dev`'s own binary, so it is not this branch's. The fault
address is `rax + 15`, which is `GC_FLAGS_BYTE_OFFSET` — the compact `getfield`
fast path's flags load through a receiver that points into the semi-space the
previous collection evacuated and handed back.

This is **exactly** the fault `uncommit_evacuated_young`'s own doc predicted, and
the flag is behaving as documented: the defect it exposes belongs to a compiled
frame, not to the give-back. It is the DEFAULT that was wrong. The evidence for
the flip was a suite that does not run this corpus, and the same measurement
that justified the flip put the benefit at 2552 ms against 2562 ms — free to
within noise. A change with no measurable benefit does not get to crash a
supported collector on a real workload by default.

Note also what this page's own ZGC fix cannot do here. Finding 7 closed the
identical shape for ZGC by withdrawing `JIT_READ_BOUNDS`, and that would not
have helped: `emit_trusted_oop_receiver_check_at` emits a bare null test and
then dereferences the receiver unconditionally, consulting no bounds table at
all. The read bound is not the only door.

The switch is now the sharpest instrument in the tree for this family — it turns
a stale young reference from a silent read of the previous cycle's bytes into an
immediate, attributable SIGSEGV with the span, the site and the code buffer
already printed. That is what found the compiled-frame record.

### `CRATONVM_GC_OBJECT_STARTS` — its 7% was a measurement artefact

Re-run as concurrent pairs, every arm checked for its probe's own success line:

| workload | pairs | `on/off` ratios | median |
|---|---|---|---|
| single-threaded churn, 96 MiB live | 6 | 0.914 1.006 0.888 1.053 0.742 1.077 | 0.96 |
| 8-thread churn, GROWING young set | 8 | 1.030 1.085 1.001 1.079 1.035 1.059 1.018 0.846 | **1.03** |

Neither reproduces the 7%. The multi-threaded, growing shape — the one this page
listed as unmeasured, in those words — is **slower in 7 of 8 pairs**. The bitmap
is rebuilt on every arena growth, which is a mechanism for that and is the path
nothing had run.

Nothing is wrong with the bitmap. What was wrong was reading a sequential ABBA
on a machine that ~20 other agents build on.

---

## The oracle's first answer was about the oracle

Worth recording because it is the third instrument on this page to have been
wrong in a way that read as a finding.

`CRATONVM_DBG_GC_TRIGGER_VERIFY=1` reported **0 divergences in 4550 checks** on
a single-threaded probe, and then **88 in 69,922** the first time it ran on
multi-threaded H2. Eighty-eight is a small number and it is exactly the shape a
missing publish would have — `used` low, `free` and `capacity` agreeing.

It was the verifier. It read the published triple and THEN took the lock, so a
peer thread allocating between the two reads produced a `published` that lagged
`actual` by one allocation. Acquire first, then read published, then read the
arena: under the lock nothing can mutate and nothing can republish, so the two
must be equal and a divergence means what it says. **63,661 checks, 0
divergences** on the same workload with the order corrected.

The tell that it was the reader and not the writer was there in the first
report and worth naming: only ONE of the three published values ever diverged,
always in the same direction, always by about one object.

## How these were measured, and why two of this page's own numbers do not stand

Every throughput row above is **concurrent paired arms**: both arms started
within two seconds of each other so they contend with each other and see the
same host in the same seconds, launch order alternated between pairs so a
start-order effect cannot masquerade as the treatment, only the ratio read, and
`/proc/loadavg` recorded per pair.

A sequential ABBA cancels a monotone DRIFT. The disturbance on this box is other
agents' builds, which is SPIKY, and a spike sitting in an ABBA's two middle
positions is arithmetically indistinguishable from the treatment — the same
design once read 1.9x for a JIT flag that costs nothing.

Two numbers on this page were taken that way and do not stand: the
`object-starts` "median ~7% faster with it on", and the `gen-uncommit` table
beside it. The give-back's "free to within noise" survives, because concurrent
pairs put it at 0.942 / 1.015 / 1.042 / 1.070 — still free, now measured under a
design that could have shown otherwise.

Two readings taken with the same wrong design during this follow-up are recorded
as refuted rather than deleted: a sequential ABBA put the lock-free GC trigger
5/5 reps faster and the object-start bitmap 3.8% slower, and concurrent pairs put
both at 1.0. The failure mode is not subtle and it is not rare.

## The engagement numbers were behind a JIT flag

`[cratonvm] static root slots: patched=…`, `exact object-start answers: hits=…`
and `generational young uncommit: … bytes` — the three lines this page quotes as
the proof that its switches engaged — printed only under
`CRATONVM_JIT_METHOD_STATS`, which is about the JIT and is named in none of
those records. A reader who set the GC switch and saw no line would have read
that as "the switch did nothing", which is the exact failure every one of those
counters was written to prevent, one level up. They are outside that gate now.

## What moved out

* The compiled-frame stale young reference — its own record, with the
  ten-second reproducer, the five-arm attribution and the
  `CRATONVM_DBG_STALE_FRAME_WORDS` witnesses naming `org/h2/command/Command.stop`
  and `org/h2/mvstore/tx/Transaction.commit`.
