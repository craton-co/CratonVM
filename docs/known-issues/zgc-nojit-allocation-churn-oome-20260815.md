# ZGC and the generational heap throw `OutOfMemoryError` on allocation churn that G1 and HotSpot survive

**Status: OPEN**, found 2026-08-15 while measuring the fragmentation cost the
retired `zgc-resourceleakdetector-corpse-read` write-up left as its open
question. Not a regression: it reproduces identically on a binary built before
that session's changes and on one built after, with byte-identical collector
counters.

## The failure

`FragProbe` — 4 000 000 allocations of 64 B .. 8 KiB through one method, with a
512-entry rolling window of live references, in a 512 MiB heap:

```
Exception in thread "main" java/lang/OutOfMemoryError: Java heap space (alloc_array length 5661)
```

A 5 661-byte array in a 512 MiB heap whose live set is a few megabytes.

| collector | `--nojit` | JIT on |
|---|---|---|
| ZGC | **OOME** | completes |
| generational (default) | **OOME** | completes |
| G1 | completes (101 blocks) | completes |
| HotSpot 25 | completes (101 blocks) | completes |

## What the collector says at the failure

```
[GC] zgc-real: collections=3 occupancy=2189368/536870912 bytes
[GC] zgc-features: compaction_cycles=2 objects_relocated=447 relocation_skipped_jit=0
```

**Three collections.** The JIT arm of the same workload runs 69. So this is not
a heap that collected repeatedly and lost the race — the VM gave up almost
immediately, having relocated 447 objects across 2 compaction cycles.

## Why the JIT arm is the one that works

`relocation_skipped_jit=0` in the failing arm and `64` in the passing one. With
`--nojit` no compiled frame is ever live, so `gc_quiescence::is_active()` is
false, so `relocate_stw` proceeds and the slide runs. With the JIT on it
declines almost every cycle. **The arm that relocates is the arm that dies**,
which is the opposite of the intuition that compaction is what saves a
fragmenting heap, and is the same direction the fragmentation numbers point in
(the retired write-up's cost table: `CRATONVM_ZGC_RELOCATE=0` ends with a
*larger* worst-case largest free block than the compacting arm).

That makes "the slide is doing something wrong to the free list" the first
hypothesis to test, not the last. `CRATONVM_ZGC_RELOCATE=0` under `--nojit` is
the one-flag discriminator and has not been run.

## Not yet established

* Whether the generational heap fails for the same reason or a different one
  that happens to share a symptom. Both were tested; only ZGC was instrumented.
* Whether the collector counters above are the failure's state or its
  aftermath — `occupancy` is a post-sweep figure read at shutdown, after the
  OOME unwound, so it is not evidence about the heap at the failing request.
* Whether a smaller heap or a different size mix moves the boundary.

## Repro

```bash
# FragProbe.java is in repros/frag-churn/ beside this page.
javac FragProbe.java
CRATONVM_GC_STATS=1 <cratonvm> --java-home <jdk-25> --Xmx 512m \
  -XX:+UseZGC --nojit -cp . FragProbe 200 20000
```

Drop `--nojit` for the passing arm; swap `-XX:+UseZGC` for `-XX:+UseG1GC` for
the other passing arm. `java -Xmx512m FragProbe 200 20000` is the HotSpot
oracle.
