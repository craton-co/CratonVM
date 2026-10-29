# `Zt1MkSystemGcFresh` `--jdk-only`: a rare survivor no diagnostic build reproduces

> **STATUS (2026-10-25, zwr): OPEN.** The residual of
> [`zgc-delivery-thread-conservative-jit-band-keeps-a-weak-referent-FIXED-20261024`](../../internal/gc/zgc-delivery-thread-conservative-jit-band-keeps-a-weak-referent-FIXED-20261024.md).
> The rate fell from 2-5 % of runs to about 0.1 %, and only in `--jdk-only`.

## The row

`Zt1MkSystemGcFresh 40 2000 6291456`, `--jdk-only`, the default collector, host 64.177.43.150 (16 cores, load
average 20-46):

* HotSpot 25 prints `alive_after_gc=0` every time.
* The landed zwr tree (b28 = dev `2447d57378` plus the zwr branch) printed `alive_after_gc=1` in 3 of 3 000 runs
  (2/1000, then 1/2000).
* `--compatible` was clean on the same tree, 1 000/1 000.

## Why no producer is named

Every build or run that carried diagnostics ran clean, 0 in 8 800:

| build | diagnostics | runs | survivors |
| --- | --- | --- | --- |
| b29 | the zwr round's temporary set, with `CRATONVM_DBG_ROOT_KIND_CENSUS=1` | 3 000 | 0 |
| b29 | the same set without the census | 2 487 | 0 |
| b28 | permanent switches only: `CRATONVM_DBG_REF_RESTORED`, `CRATONVM_DBG_OLDMARK_ROOT_CENSUS`, `CRATONVM_DBG_FRAME_ROOT_UNNAMED` | 3 346 | 0 |

The temporary set was a root-path tracer, frame-scan notes, take-over source tags and companion trigger tags.
Every earlier producer of the round was a stale word on the reference-delivery thread ("Craton Finalizer"), and
each was rarer than the one before it: about 1 in 300, then 1 in 1 000, then 1 in 3 000. So this one is
probably the same family. Its occurrence depends on timing, and every diagnostic so far shifted that timing.

## What would close it

* A cheaper hold-the-evidence diagnostic: record, at each `System.gc()` pause, the root word and its source for
  every root that names an object a `WeakReference` registered this cycle references. Keep the records in a
  ring and print them only when the restore pass keeps such a referent, so nothing is printed on a clean run.
* Then the rule for that holder, in the shape of the round's five
  (`with_chain_covered_java`, the published Rust band floor, the per-notification scrub, the ZGC above-chain
  starts rule, the innermost-frame split).
* Close: `--jdk-only` 0 survivors in 3 000 runs of the landed binary.
