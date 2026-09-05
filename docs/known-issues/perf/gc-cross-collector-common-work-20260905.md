# The work that is common to all three collectors — 2026-09-05

A review of `gc/` asking only one question: **what is shared by Generational,
G1 and ZGC, and what is wrong with it?** Eight findings, what landed against
each, and — for the ones that did not land in full — what a soak would have to
answer first.

The recurring shape is worth stating up front, because it predicts most of the
list: **ZGC's modules are where the measurements happened, and almost nothing
learned there was carried back.** Three of the eight findings are literally
"the good implementation exists, inside `#[cfg(feature = "zgc")]`".

---

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

**Not landed:** the actual fix — an allocation-time object-start bitmap, which
makes the predicate one shift and one bit test, exact, with **no** false
positives, and removes the entire "plausible header" heuristic family. The
enabling refactor is finding 4's `HeapBitmap`. What remains is per-collector:
allocation-time `insert` and sweep-time `remove` in both Generational and G1,
which is a change to the single most safety-critical predicate in the GC and
needs a soak with an engagement census (bitmap-answered vs heuristic-answered)
before it can be trusted. Shipping it unsoaked would be the wrong trade.

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
The accurate statement is: **ZGC does it by default, G1 can on request, and the
Generational collector cannot at all** (no `decommit`/`uncommit` call anywhere in
`gen_heap.rs`, and `OldGen` is a plain `Vec<u8>` — wholly committed at startup,
with no reservation to shrink).

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

**Not landed:** generational uncommit. It needs `OldGen` to become
reservation-backed (`Vec<u8>` → `HeapStore`, 33 `self.data` sites) so it can
prefix-shrink the way G1 does. Interior decommit is *not* the shortcut: it walks
straight into the hazard above.

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

## What a follow-up should measure first

1. Re-run the **+153 % four-worker mark** number now that the provider lock is
   gone. Everything in finding 6's second half depends on whether it survived.
2. A per-cycle **`MarkBitmap::clear` timing** on a G1 workload with many freed
   regions — the `any_marked` early return should show up as a step change in
   cleanup cost, and if it does not, the bitmap was not where the time went.
3. `CRATONVM_DBG_STATIC_SLOT_VERIFY=1` over a real application (H2 corpus,
   Tomcat) before anyone builds further on finding 1's slot path.
4. The **engagement census** for a bitmap-backed `is_object_address` — how many
   probes the bitmap answers versus the heuristic — before finding 5's real fix
   is written, not after.
