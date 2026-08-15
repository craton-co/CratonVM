# ZGC's reference-processor tables were never remapped after a slide — FIXED 2026-08-15

**One line:** ZGC is the only backend that owns a live `ReferenceProcessor`, it
stores raw addresses, and nothing ever called `update_after_gc` on it — so
after compaction shipped, `process_references` wrote null into field 0 of
whatever object had moved into a relocated `Reference`'s old address.

This closed `io.netty.util.ResourceLeakDetectorTest`, the last open member of
the ZGC-only SIGSEGV cluster (`zgc-specific-sigsegv-cluster-20260814.md`), and
turned up a fifth defect of the same family alongside it.

## The signature, and why it carried no information

```
zgc real: field index OOB index=0 num_slots=0
EXCEPTION_ACCESS_VIOLATION (SIGSEGV), faulting read at 0x0F
  at io/netty/util/ResourceLeakDetectorTest$1.run
  at io/netty/util/ResourceLeakDetector$DefaultResourceLeak.<init>
```

`compact_low_to` **zeroes the span it vacates**, deliberately, so a
conservative scan cannot resurrect a corpse. The side effect is that a stale
pointer resolves to a well-formed ALL-ZERO object — `class_id=0`,
`num_slots=0`, no fields — and every identifying byte is gone by the time
anyone notices. Three investigations had reached "something holds a stale
address" and stopped there, because the evidence is destroyed by the same
memset that makes the bug survivable.

## What actually happened

`ZgcRealHeap.ref_processor` holds `reference_obj`, `referent` and `queue_addr`
as raw `usize` for every discovered soft/weak/phantom/cleaner/finalizer
`Reference`, plus the soft-ref address index and the finalization queue. These
are **collector-side tables, not Java reference slots**, so
`relocate_and_compact`'s rewrite pass cannot see them. The only route from the
pointer map to them is `ReferenceProcessor::update_after_gc`.

Nothing called it. `process_references` said why:

> (Non-moving: addresses are stable, so no `update_after_gc` relocation is
> needed.)

True when written; false from 2026-08-13, when compaction went default-on.

Unlike the three earlier defects of this family, **this one persists across
cycles.** The tables outlive the collection, and cycle N+1's pointer map does
not contain cycle N's pre-move addresses, so a missed entry is stale for the
rest of the process. Both branches then cost something:

* `is_marked_addr(stale)` reads the zeroed span, finds no mark bit, and
  `remove_collected` drops the entry — a **live** `WeakReference` silently
  leaves the processor and is never cleared or enqueued. No `Cleaner`, no
  queue delivery.
* If a new object has since been allocated at that address and IS marked,
  `process_references` writes null into **field 0** of an unrelated live
  object. When the new occupant has no slots, that is the `index=0
  num_slots=0` warning above.

`ResourceLeakDetector$DefaultResourceLeak` extends `WeakReference`, so netty's
leak detector is a table of exactly these objects. That is why one netty class
crashed and 656 did not.

## The instrument that named it

`CRATONVM_DBG_ZGC_CORPSE=1` keeps a ledger of what each slide vacated —
destination, class id, size, and which cycle — and reports it when a read
lands on a zeroed span. One run:

```
zgc corpse read: this address was vacated by the LAST slide
  read_addr=2733782942912  vacated_base=2733782942912  interior_offset=0
  moved_to=2733782371072   size=176   cycles_ago=0     index=0
  class=io/netty/util/ResourceLeakDetector$DefaultResourceLeak
  survivor_still_registered=true
```

Every field earns its place. `class` names the holder family. `index=0` is the
referent slot, which is what `process_references` writes.
`survivor_still_registered=true` says the object is alive at `moved_to`, so
this is a **missing remap and not a lifetime bug** — the distinction that had
been unavailable. `cycles_ago=0` places the read in the cycle right after the
slide.

The ledger accumulates across cycles rather than keeping only the last one: a
holder that reads in the same cycle and one that cached the address ten
collections ago produce the identical warning and are different bugs.

Backtrace capture is included but was **not** what solved it — fat LTO plus
`debug = "line-tables-only"` renders every frame `<unknown>`. The VM's own
crash dump gave the Java frames instead. Worth knowing before relying on
`Backtrace::force_capture` in a release build of this tree.

## The fifth defect, found by asking the question rather than hitting it

Once the fourth was understood the general question is obvious: **which other
ZGC-internal tables hold raw addresses across a slide?** Auditing the fields
of `ZgcRealHeap` found one more live defect.

`resurrected_finalizers` is filled during the remark, several phases before the
slide, and was never rewritten — under a comment reading `// non-moving:
address unchanged`. `collect_garbage_with_finalizers` returns that list and the
runtime enqueues each entry on the `FinalizerThread`; G1 documents the contract
outright as "the POST-copy addresses of objects Phase 3.5 resurrected this
collection". A resurrected object is live by definition, so it is exactly what
the slide relocates, and `finalize()` would then run against a vacated span.
Measured: **73 of 88** reported at a dead address in a single collection.

Cleared by the same audit, each for a specific reason and not by inspection
alone:

| table | why it is safe |
|---|---|
| `critical_pins` | pinned objects' pages are withheld from the relocation set, so they do not move |
| `pending_finalizer_roots` | `mem::take`n during the remark, pre-slide |
| `pending_queues` | filled and drained inside one `process_references` |
| `soft_ref_lru_index` | keyed by position, and relocation is in-place |
| `tlabs` | `retire_all_tlabs()` runs inside `collect_garbage`; `tlab_reserved_tails` is an existing tripwire |
| `forwarding` | gated on `relocate_active`, the concurrent-relocation scaffolding, not the STW slide |
| `remembered` | `remembered_roots` re-checks the registry, so a card naming a vacated address is skipped (under-approximates; only meaningful once generational ZGC ships) |

## Tests

`a_relocated_reference_is_reachable_at_its_new_address_in_the_processor` —
84 of 106 registered Reference objects stale per collection without the fix.

`a_resurrected_finalizable_object_is_reported_at_its_post_slide_address` —
73 of 88 without the fix.

Both assert an END STATE (every stored address resolves to a live allocation
base) rather than counting remap calls, and both assert the fixture actually
relocated something before judging it.

### Two ways the second test was wrong first

**Occupancy.** 1-in-5 rooted plus 1-in-5 finalizable leaves each page 40% live,
above the selector's `max_live_occupancy` of 0.25 — so it declines every page,
nothing moves, and the test passes for a reason unrelated to the fix. It only
surfaced because the non-vacuity guard was written first. **A relocation test
on this collector must keep total occupancy under 25% or it tests nothing.**

**Where non-vacuity is measured.** Deriving "did anything move" from the
returned list is a trap: with the remap missing, that list holds PRE-slide
addresses, so the count of moved entries is zero and the test fails on its own
vacuity guard — which reads as a broken fixture rather than as the defect. Both
red-checks initially reported the wrong thing for this reason. Measure motion
from the **pointer map**, which is the collector's own record and is unaffected
by the bug under test.

## The family, now five

Every one is code that was correct while the collector never moved an object:

1. `pin_critical_region` pinned nothing under ZGC (memory corruption)
2. the reference-processing guard tested the pre-move address (silent loss)
3. `prune_dead` ran with pre-slide addresses (memory corruption)
4. the reference processor's tables were never remapped (this one)
5. `resurrected_finalizers` reported pre-slide addresses (this one)

Plus ZGC never consuming `pinned_jit_roots_snapshot()`.

The shape to search for is not "which arms return a constant" but **which code
answers a question about an ADDRESS** — and, added by this pair, **which
collector-side table OUTLIVES the cycle that filled it**. 1-3 are all
within-cycle and were found by their crashes. 4 and 5 are cross-cycle, which is
why they were the ones left.
