# Heap substrate — proposals from the 2026-09-20 GC round

Scope: the `VmHeap` dispatcher, object layout, reservation/commit, the bitmaps,
metaspace and NUMA. Everything below is sized, and every item names its first
step. Ordered by (value ÷ risk), not by size.

---

## 1. A drift gate for the `VmHeap` dispatcher — `HIGH VALUE, SMALL`

**The problem this round kept re-discovering.** `gc/src/vm_heap.rs` is ~5.9k
lines of three-armed `match`, and the recurring defect is not a wrong arm — it
is an arm that answers a constant (`false`, `0`, `None`, `Vec::new()`,
`(0, 0)`) long after the backend behind it grew the thing the constant denied.
Every instance the file's own comments record has the same shape, and the same
half-life of months:

* `young_spill_pressure` ZGC arm — hardwired `false`; the collector `abort()`ed
  on a heap full of garbage. Fixed 2026-08-07.
* `note_young_spill_pressure` / `clear_young_spill_pressure` ZGC arms — no-ops
  under a `TODO(zgc)` that had already been discharged.
* `enable_gc_logging` ZGC arm — printed a claim instead of enabling anything;
  `--verbose:gc -XX:+UseZGC` produced nothing for a whole run.
* `conservative_addr_span` ZGC arm — `None` on a premise ("ZGC keeps live bases
  in a registry, not a contiguous arena") that was false when it was written.
  Its own comment calls that "the reason this went unfixed".
* `get_array_element_unboxing` ZGC arm — a genuine fallback, which is why
  `Stream.mapToLong(..).toArray()` returned zeros under `-XX:+UseZGC`.
* `reclaimed_hole_at` ZGC arm — `None` on G1's reason, which did not apply.
* `is_addr_live` ZGC arm — the loose predicate, which was unsound.

Seven, all one backend, all found by a human reading the file end to end. That
is not a review problem, it is a missing instrument.

**Proposal.** A test in `gc/tests/` that parses `vm_heap.rs` and, for every
`pub fn` whose body is a `match self` over `VmHeap`, classifies each arm as
DELEGATING (calls a method on the bound heap) or CONSTANT (returns a literal /
`Default::default()` / an empty collection / `{}`). It emits a census and
ratchets the CONSTANT set against a checked-in baseline file, exactly as
`native-builtins/tests/stub_ratchet.rs` freezes the synthetic-stub baseline
with zero slack. A new constant arm, or a delegating arm that becomes constant,
fails CI; converting a constant arm to a delegation requires deleting its
baseline line, which is the moment somebody reads the justification.

It is a source-scanning gate, so it is a parser and will be wrong in both
directions — the baseline file is what makes that tolerable: a false positive
costs one line.

**Size:** ~250 lines of test plus a ~150-line baseline. **First step:** write
the classifier and print the census without asserting; the census alone is the
first end-to-end answer to "how much of this dispatcher is inert on which
backend", which nothing in the tree currently answers.

> **First step LANDED 2026-09-20** — `gc/tests/vm_heap_dispatch_census.rs`.
> Classifies every arm of every `match self` over `VmHeap` as DELEGATING /
> CONSTANT / UNCLEAR and prints the census; `dispatch!` bodies are counted
> separately as uniformly delegating. **No baseline and no ratchet**, by
> design: a gate that asserts on day one against a number nobody has read is a
> chore with a deadline. Its only assertions are vacuity floors on the parser
> (set far below the real numbers), because a source scanner that matches
> nothing prints a clean report of a file it never read.
>
> Run it with `cargo test -p cratonvm-gc --test vm_heap_dispatch_census --
> --nocapture`. **Next step:** read the CONSTANT column, delete the rows that
> are wrong, then freeze what is left as the baseline.

---

## 2. `-Xms` for all three backends — `HIGH VALUE, MEDIUM`

Filed as
[`internal/gc/heap-xms-xmx-mean-different-things-per-backend-20260920-RETIRED-20260921.md`](../internal/gc/heap-xms-xmx-mean-different-things-per-backend-20260920-RETIRED-20260921.md)
(then `known-issues/gc/heap-xms-xmx-mean-different-things-per-backend-20260920.md`).
`-Xms` reaches G1 only, and it reaches G1 through a field on
`G1ConfigOverrides` — which is why the other two arms could not read it even in
principle. The mechanism already exists: `HeapStore` reserves and commits in
2 MiB granules, and both `Arena` (Generational) and `ZgcRealHeap` sit on it.

**Size:** one signature change per backend constructor plus one
`commit_range` at construction; ~120 lines. **First step:** move
`initial_heap_size` off `G1ConfigOverrides` into a parameter of
`VmHeap::new_with_overrides`'s three arms — a mechanical change that makes the
gap a compile error in the two arms that currently cannot see it.

> **First step LANDED 2026-09-20.** `-Xms` is a parameter of
> `VmHeap::new_with_heap_sizing`, and each arm returns an `XmsDisposition`, so
> an arm that drops the flag has to say so. The launcher warned on a dropped
> `-Xms`.
>
> **CLOSED 2026-09-21.** All three arms now return `Committed`:
> `Arena::commit_initial_prefix` got its two callers —
> `ZgcRealHeap::with_capacity_and_initial` (a prefix commit, never a smaller
> reservation; the arena envelope is captured once and read lock-free) and
> `GenerationalHeap::commit_initial_heap` (the young pair; the old generation
> is a wholly-committed `Vec` and contributes its share whatever `-Xms` says).
> The launcher's warning is gone, because nothing drops the flag any more.
> `VmHeap::os_committed_bytes` is the instrument the page said was missing —
> `committed_bytes` is `Runtime.totalMemory()` and is a sum of capacities, so
> it could not tell an honoured `-Xms` from a dropped one. Retired to
> [`internal/gc/heap-xms-xmx-mean-different-things-per-backend-20260920-RETIRED-20260921.md`](../internal/gc/heap-xms-xmx-mean-different-things-per-backend-20260920-RETIRED-20260921.md).
>
> **Two corrections to the sizing claim above**, both from reading the
> constructors rather than the page: (1) `Arena` and therefore `ZgcRealHeap`
> reserve rather than commit, so ZGC does *not* take `-Xmx` in commit charge at
> startup; (2) `OldGen` is **not** on `HeapStore` — it is a raw
> `Vec::with_capacity` of half of `-Xmx`, which is the one place in the
> Generational heap that really is charged up front. The remaining `-Xms` work
> and the ZGC design decision are written out on the known-issues page.

---

## 3. Make the `HeapBitmap` family one family — `MEDIUM VALUE, SMALL`

`gc/src/heap_bitmap.rs` is documented as "a strict superset of all three" of
`young_mark::ObjectStartBits`, `young_mark::YoungMarkBits` and
`mark_bitmap::MarkBitmap`, and `MarkBitmap` has already been reduced to a
vocabulary over it. The other two are still independent implementations —
one `&mut self` over a `Vec<u64>`, one atomic but deliberately NOT
alignment-exact ("`addr` and `addr + 4` map to the same bit"). The second is
the shape that `MarkBitmap`'s own comment says "a bug becomes impossible rather
than merely unlikely" to remove.

The interesting part is not the deduplication, it is that the two survivors are
precisely the ones whose callers "pre-screen" — an unwritten precondition held
by two collectors and checked by neither.

**Size:** ~200 lines moved, most of it deletion. **First step:** add a
`debug_assert!(addr & 7 == 0)` to `YoungMarkBits::locate` and run the gc suite.
If it never fires, the pre-screen claim is true and the swap is safe; if it
fires, that is the finding and the swap is urgent.

---

## 4. Charge `is_object_address` to its callers — `MEDIUM VALUE, SMALL`

`VmHeap::is_object_address` is described in its own body comment as carrying a
TOTAL walk counter so the per-site JIT census can be checked against it — but
the counter is not there; the comment is all that remains. Meanwhile the file
has grown `class_id_of_validated`, `kind_of_validated`,
`element_type_of_validated` and `load_and_forward_validated`, every one of them
an A/B against `CRATONVM_GC_NO_VALIDATE_ONCE`, and the note on
`validate_once_enabled` records that the change "landed with a walk count and
no wall clock, and on this path those are not the same measurement".

**Size:** ~40 lines. **First step:** restore the total counter as a relaxed
`AtomicU64` behind the existing `CRATONVM_DBG_*` gating and print it beside the
per-site census, so the next `_validated` twin is priced against a denominator
instead of against its own numerator.

---

## 5. Retire or adopt `compact_header.rs` — `MEDIUM VALUE, LARGE`

2,434 lines implementing JEP 519 / Lilliput headers, with a forwarding-overflow
side table, a hash-code side table and a narrow-klass table. **No collector
uses it**: `VmHeap::get_compact_header` is the only in-crate reader and its own
doc says every backend lays out the full 32-byte `ObjectHeader`, so the method
"has no in-tree callers today". `clear_forwarding_overflow` is documented as
"NOT CALLED YET — this is an obligation, not a description", and its tokens are
minted monotonically and never reclaimed.

This is not dead weight to delete on sight: `compressed_oops.rs`'s own measured
conclusion is that the 32-byte header, not narrow oops, "is the item with the
leverage here" — 4.7 % of peak RSS for compression against 24 bytes per object
for the header. So this module is the high-value item, parked.

**Size:** adoption is a multi-session project (every header read in three
collectors). **First step, one session:** make the parked state honest —
`FORWARDING_OVERFLOW` interns a fresh token per `set_forwarding_ptr` call with
no dedup and no caller for `clear`, so wire the clear into the one place that
would own it *and* add the test that fails if it is not called. A side table
that only grows is the part of a parked feature that becomes a leak the day it
is switched on.

---

## 6. Give `metaspace.rs` a caller or a retirement date — `LOW VALUE, MEDIUM`

`gc/src/lib.rs` already answers the grep: `metaspace` (1,623 lines) and
`class_unloading` (1,420) have no caller anywhere in the workspace, class
metadata is actually unloaded by `vm/src/memory/gc.rs`, and "metaspace" as a
Java program observes it is an approximation computed in `vm_init.rs` from a
live class count times an estimated per-class overhead. So
`-XX:MaxMetaspaceSize` bounds nothing and there is no
`OutOfMemoryError: Metaspace` — while `metaspace.rs` contains a working
`max_metaspace_size` bound and the error path to go with it (`:478`).

The cost of the current state is not the dead lines, it is that the module reads
as coverage. **First step:** decide, and record the decision where the flag is
parsed rather than where the module is defined — either `vm_init` consults
`MetaspaceConfig::max_metaspace_size` for its approximation, or
`-XX:MaxMetaspaceSize` warns that it is accepted and inert.

---

## 7. NUMA: detect-only is fine; say so at the flag — `LOW VALUE, TINY`

`numa.rs` is honestly documented ("**No NUMA placement happens. This module
only *detects*.**") and that honesty is why this is a one-liner rather than an
investigation. The 2026-09-20 session removed the one real cost — a
`/proc/self/status` read and parse per slow-path allocation on multi-node Linux
hosts, feeding a `tracing::trace!` that is compiled out of release — by
memoising `current_thread_node` per thread.

What remains is `NumaAllocator` / `NumaArena` / `NumaAllocation` / `NumaStats` /
`NumaPolicy` and `StringDeduplicator`: no caller, and `NumaAllocator::allocate`
is a simulation that bumps an integer from `0x1000` and never reports a
cross-node allocation. **First step:** delete the simulation half. A type named
`NumaAllocator` that allocates nothing is the thing a future reader has to
disprove before they can start.
