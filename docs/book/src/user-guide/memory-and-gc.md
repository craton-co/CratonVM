# Memory & Garbage Collection

CratonVM manages the Java heap with a **generational garbage collector** and
sizes the heap ergonomically when you don't specify a size. This chapter is
about *operating* the heap — sizing it, choosing a collector, and diagnosing
pauses. For how the collector works internally, see [The Garbage
Collector](../internals/garbage-collector.md).

## Heap sizing

### The ergonomic default

When you do **not** pass `-Xmx`, the launcher chooses a default maximum heap
based on the machine, rather than a fixed small number:

- **Fraction:** about **¼ of physical RAM** (approximating the stock JDK's
  `-XX:MaxRAMPercentage=25`).
- **Floor:** 256 MiB — the ergonomic default only ever *raises* the heap above
  the historical baseline, never lowers it.
- **Cap:** 4 GiB by default. The cap exists because the generational heap
  *eagerly commits* its arenas, so an uncapped ¼-of-RAM heap on a large host
  would charge that much memory per process. Override the cap with
  `CRATONVM_DEFAULT_HEAP_MAX_MB=<N>` (MiB).
- **Fallback:** if physical RAM cannot be probed, the fixed 256 MiB default
  stands.

This matters for real frameworks: at a fixed 256 MB, allocation-heavy apps
(build tools, dependency-injection containers, test runners) thrash the
collector and can look like a hang, where a JDK that auto-sizes finishes fine.

Precedence the launcher applies:

```text
-Xmx  →  ergonomic default (¼ RAM, capped)  →  256m
```

### Setting the heap explicitly

```bash
cratonvm --Xmx 2g  --classpath . MyApp
cratonvm --Xmx 512m --classpath . MyApp
```

`-Xmx` accepts `k`, `m`, and `g` suffixes and always wins over the ergonomic
default.

`-Xms` (initial heap) is accepted for HotSpot compatibility but currently
ignored.

### Overriding ergonomics

| Goal | How |
|------|-----|
| Pin the heap | `--Xmx <size>` |
| Cap the ergonomic default | `CRATONVM_DEFAULT_HEAP_MAX_MB=<N>` (MiB) |
| Disable ergonomics (back to 256 MB) | `CRATONVM_DEFAULT_HEAP_ERGONOMICS=0` |

With `--verbose:gc`, the chosen default is printed at startup, e.g.:

```text
[cratonvm] ergonomic default max heap: 4096 MB (1/4 physical RAM; set -Xmx or CRATONVM_DEFAULT_HEAP_ERGONOMICS=0 to override)
```

## Choosing a collector

| Collector | How to select | Status |
|-----------|---------------|--------|
| **ZGC** (default since 2026-08-10) | (default), `-XX:+UseZGC`, or `--XX:UseGc ZGC` | **The default.** Real and wired end to end, but **not** a real ZGC: a memory-backed, non-moving mark-sweep over one arena. It **can be generational** since 2026-08-17 (opt-in, `CRATONVM_ZGC_GENERATIONAL=1`, split by object age) and its **marking can be concurrent** since 2026-08-16 (opt-in, `CRATONVM_ZGC_CONC_START=60`: per-cycle pause -38% to -58%, wall clock +37% to +55%) and whose sweep is stop-the-world. It has thread-local allocation buffers (default-on). Budget ~1.5x the heap a compacting collector needs. |
| **Generational** | `-XX:+UseGenerationalGC` or `-XX:-UseZGC` | Stable. Young/old generations, write barriers, card table. Moving (Cheney) young copy plus non-moving sweep with selective promotion. The fallback whenever ZGC is not compiled in. |
| **G1** (region-based) | `-XX:+UseG1GC` | **Experimental.** Region-based collector; the generational collector remains the safety net during its maturation. |

The `zgc` Cargo feature is **on by default** — it gates the `GcAlgorithm::Zgc`
variant, so the default could not be `Zgc` without it. Only a
`--no-default-features` build lacks ZGC; there `-XX:+UseZGC` warns and falls
back to Generational, like any unknown collector name.

What ZGC here does **not** have is everything the OpenJDK name promises: no
colored pointers, no load barriers, no concurrency, no compaction, no
generations. It *does* have thread-local allocation buffers — its own, inside
the backend (`CRATONVM_ZGC_TLAB=0` turns them off) rather than through
`VmHeap::refill_tlab`, which still returns `None` for this backend. Any page
telling you ZGC allocates by taking the arena lock on every allocation is
describing the collector as it stood before 2026-08-08.

The practical consequence of not compacting is **headroom**: free memory can be
plentiful and still too broken up to serve one large array. See
[GC tuning](../../../gc-tuning.md) for the sizing guidance and
[the maturity assessment](../../../feature-designs/zgc-maturity-assessment-and-plan-20260813.md)
for what is built, what is not, and the plan to close the gap.

```bash
# Default (ZGC)
cratonvm --classpath . MyApp

# The generational collector
cratonvm -XX:+UseGenerationalGC --classpath . MyApp

# Opt into the experimental G1 collector
cratonvm -XX:+UseG1GC --classpath . MyApp
```

Unknown collector names produce a warning and fall back to Generational.

## Watching the collector

```bash
cratonvm --verbose:gc --classpath . MyApp
```

For HotSpot-style structured GC logging, use unified logging:

```bash
cratonvm --Xlog "gc*=info:stdout:time,level,tags" --classpath . MyApp
```

## Out-of-memory diagnostics

If a program exhausts the heap you'll get an `OutOfMemoryError`. To capture an
HPROF heap dump for analysis:

```bash
cratonvm -XX:+HeapDumpOnOutOfMemoryError -XX:HeapDumpPath=./oom.hprof \
         --classpath . MyApp
```

If you simply need more memory, raise `-Xmx`.

## When to tune

- **Frequent GC pauses / poor throughput on allocation-heavy code:** raise
  `-Xmx`. The default collector triggers young collection as the young region
  fills; a larger heap reduces collection frequency.
- **Running in a container:** the ergonomic default is based on *physical RAM*,
  not the cgroup limit, so inside a memory-constrained container you should set
  `-Xmx` explicitly (roughly 50–75% of the container limit) or cap it with
  `CRATONVM_DEFAULT_HEAP_MAX_MB`. See [Containers & cgroups](containers.md).
- **Deep recursion hitting `StackOverflowError` unexpectedly:** that's the call
  stack, not the heap — raise `RJ_MAX_STACK_DEPTH` and/or `RUST_MIN_STACK`.
