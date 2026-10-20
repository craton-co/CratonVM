> **STATUS (2026-10-19, zt t5/gn): OPEN, unchanged -- awaiting the HotSpot row (`Zt3GnYoungRefs 256 -Xmx1g`; not in the t5 rows).** Two additions by reading. (1) The young cycle's reference work is larger than this page said: the pre-collection pass (`weakref_null_referents_pre_gc`) nulls every active weak referent and the post-collection restore writes each survivor back through `set_field_suppress_satb`, which cards the `Reference` when it is old -- so every old `Reference` with a live referent is re-carded, and re-scanned by the next young cycle, on every young cycle. (2) A clear-all-soft request consumed by a young cycle did nothing for old soft referents; fixed separately (`CRATONVM_ZGC_GEN_SOFT_CLEAR_MAJOR`, t5/gn), and that fix is independent of this page's reading.

> **STATUS (2026-10-19, zt t5/pg3): OPEN (a local HotSpot row decides).** Run `tools/probes/Zt3GnYoungRefs.java` against `java -XX:+UseZGC` on the Windows box, default and `CRATONVM_ZGC_GENERATIONAL=1`; if HotSpot's minors keep young referents, gate reference processing and finalizer resurrection off young cycles in `collect_garbage` (gc/src/zgc.rs, the PHASE G young path).

> **STATUS (2026-10-19, zt t3/gn): OPEN -- a reading to be decided by a HotSpot row, no code.** Generational mode only (`CRATONVM_ZGC_GENERATIONAL=1`, opt-in today; a blocker for stage S3 of `docs/internal/proposals/gc/zgc-zt3gn-proposal-generational-as-the-default-20261019.md`). Deciding probe `tools/probes/Zt3GnYoungRefs.java` (new).

# ZGC generational: a young cycle clears weak references and finalizes young objects, which (by reading) HotSpot 25's minor collection does not

## What CratonVM does

A young cycle is the whole-heap collection with the old generation pre-marked
(`collect_garbage`, the PHASE G note above `addr_is_young`, `gc/src/zgc.rs`). Every downstream
phase runs unchanged on it, including:

- the reference processor (`process_references`, which asks `is_marked_addr`): an unmarked
  YOUNG referent of a `WeakReference` / `SoftReference` (LRU policy) / `PhantomReference` is
  cleared and its reference enqueued at the first young cycle after it became unreachable;
- the finalizer resurrection pass (`pending_finalizer_roots`, the `fin_candidates` loop after the
  mark): a dead young finalizable object is resurrected and queued for `finalize()` by a young
  cycle.

## What HotSpot 25 does (a reading, not yet a row)

In generational ZGC the reference processor belongs to the old generation
(`ZGenerationOld`'s `ZReferenceProcessor`); a young marking follows a `Reference`'s `referent`
field like any other field, so a young referent is kept (and aged, and eventually promoted) by
minors, and is discovered and cleared only by a major's old marking. Final references are
discovered the same way. If that reading is right, then under HotSpot:

- `new WeakReference<>(new int[16])` with the referent dropped survives any number of minor
  collections and is cleared by the next major (or `System.gc()`);
- a dead young finalizable object is finalized only after a major.

The reading is from memory of the JDK sources, not from a quote; it must be confirmed by the
HotSpot twin row below before anything is changed.

## Why it matters

- **Java-visible:** `WeakReference.get()`, `ReferenceQueue.poll()`, `WeakHashMap.size()` and
  finalizer timing differ between CratonVM's generational mode and HotSpot after a minor. Every
  program that counts cleared weak references after churn without a `System.gc()` sees it.
  (Programs that call `System.gc()` first are unaffected: since zt t3/gn that is a major on both.)
- **CPU:** a young cycle builds the reference skip set (`ref_skip_objs`, one `FxHashSet` insert per
  registered `Reference`) and runs the processor over every registered `Reference`, old ones
  included. A young cycle that treated references as strong would pay neither.
- **Default mode:** the non-generational default clears at every collection, which HotSpot does
  only at majors; a generational default with this fixed is the closer one.

## What would close it

If `Zt3GnYoungRefs` confirms the reading: on a young cycle, mark through `Reference` referents
(an empty skip set for young referents), skip `process_references` and the finalizer pass, and
keep the soft-reference clock untouched; old `Reference`s are pre-marked and wait for a major as
they do today. Opt-in first (`CRATONVM_ZGC_GEN_YOUNG_REFS_STRONG`), with these rows:

| row | command | pass |
|---|---|---|
| HS | `java -XX:+UseZGC -Xmx1g Zt3GnYoungRefs 256` | expected `young_window kept=true` / `after_system_gc cleared=true` (if `kept=false`, the reading is wrong: close this page as REFUTED) |
| D | `cratonvm -Xmx1g Zt3GnYoungRefs 256` (both modes) | prints `kept=n/a` (no minors: DIFF, the JF4 shape) |
| G | `CRATONVM_ZGC_GENERATIONAL=1 cratonvm -Xmx1g Zt3GnYoungRefs 256` | today `kept=false` (DIFF); SAME after the fix |
| reg | `Zt1MkSystemGcFresh`, `Zr1RfSoftLruProbe`, `tools/probes/interp/L5/SoftRefBeforeOome`, `Zs6NoSoftNearFull 90/95` on G | SAME as before |

Positive control: `kept=n/a` means the window saw a major or no minor -- raise `mb` or `-Xmx`
until G prints a boolean, or the row proves nothing.
