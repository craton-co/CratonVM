# ZGC-real vs. Generational — first full Spring Boot suite comparison, RETIRED 2026-08-10

**Status: CLOSED.** Superseded twice over, and now root-caused. This page was
the first-ever ZGC full-suite comparison; it ran against a binary that also
carried a near-total heap-corruption bug (fixed 2026-08-07 by the inline
allocator's unconditional mark-word write and the thin-unlock quartet clobber),
so its 50-class delta is not a usable measurement of the collector. The clean
rerun that replaced it is
[`zgc-real-fullsuite-regression-RETIRED-20260808.md`](zgc-real-fullsuite-regression-RETIRED-20260808.md),
and that page's two root causes explain this one's residue as well.

Kept because it is cited from a dozen places in `gc/src/zgc*`, `docs/GC.md` and
the ZGC implementation plan as the record of what the backend cost when it was
first measured, and because two of its own observations turned out to be right
for reasons it could not see.

## What it got right

* **`VirtualZipDataBlockTests`.** The page called it *"a fixture/working-
  directory or zip-construction content bug, not memory corruption or a
  collector artifact"* — and refused to attribute it to the collector on the
  evidence it had. Correct on both counts. The mechanism is
  `centralRecordPositions.stream().mapToLong(Long::longValue).toArray()`
  returning `[0]` through a reference-array store with no un-boxing read half
  (§1 of the 08-08 page), so the virtual zip read its entry names from the
  wrong file offset. Nothing to do with marking, sweeping or fragmentation.
* **The `zgc` cargo feature did not compile at all** before that session
  (`E0063: missing field layout_domain`), which is why no prior data existed.
  The fix is still in place, and `ci.yml`'s `experimental-features` job now
  builds the ZGC-capable launcher so it cannot silently rot again.

## What it got wrong

The "35 classes PASS -> HANG" hypothesis — *"a full stop-the-world,
whole-arena mark-sweep on every collection … is the textbook shape that turns
many short `ApplicationContext` lifecycles into a throughput cliff"* — was
never confirmed, and the clean rerun did not reproduce the cliff: 8 HANGs under
ZGC against 13 under Generational, i.e. **fewer**. The page said as much about
its own evidence ("no GC-count/pause-time instrumentation was pulled from any
of these 35 logs this round"); the honest reading is that most of that column
was the heap corruption the binary carried, not the collector.

The `PASS -> FAIL` group is the reference-array auto-box hole (08-08 page §1)
for the stream-shaped classes, and the stale generated-`$ProxyN` cache
(08-08 page §2) for the `ClassCastException: ? cannot be cast to …` ones.
Neither is in the collector.

## Affected classes

See the 08-08 page. Nothing on this page needs re-running: its binary no longer
exists in a form worth measuring.
