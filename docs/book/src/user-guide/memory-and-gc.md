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

`-Xms` (initial heap) is the memory committed at startup, and it is honoured on
all three collectors: `-Xmx` sizes the address-space *reservation* and `-Xms`
the prefix of it that is committed before the program runs. An `-Xms` above
`-Xmx` is clamped rather than refused.

On the Generational collector `-Xms` commands the young pair only. Its old
generation is committed in full at construction, so the startup commit there is
at least half of `-Xmx` whatever `-Xms` says.

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
| **ZGC** (default since 2026-08-10) | (default), `-XX:+UseZGC`, or `--XX:UseGc ZGC` | **The default.** Real and wired end to end, but **not** a real ZGC: a stop-the-world mark-sweep over one arena that **does compact, by default** (`CRATONVM_ZGC_RELOCATE=0` is the kill switch; the large-object end has its own, `CRATONVM_ZGC_HIGH_COMPACTION=0`). It **can be generational** since 2026-08-17 (opt-in, `CRATONVM_ZGC_GENERATIONAL=1`, split by object age) and its **marking can be concurrent** since 2026-08-16 (opt-in, `CRATONVM_ZGC_CONC_START=60`: per-cycle pause -38% to -58%, wall clock +37% to +55%); the sweep and the slide are stop-the-world in every configuration. It has thread-local allocation buffers (default-on). Size it as you would any compacting collector — see the headroom note below. |
| **Generational** | `-XX:+UseGenerationalGC` or `-XX:-UseZGC` | Stable. Young/old generations, write barriers, card table. Moving (Cheney) young copy plus non-moving sweep with selective promotion. The fallback whenever ZGC is not compiled in. |
| **G1** (region-based) | `-XX:+UseG1GC` | **Experimental.** Region-based collector; the generational collector remains the safety net during its maturation. |

The `zgc` Cargo feature is **on by default** — it gates the `GcAlgorithm::Zgc`
variant, so the default could not be `Zgc` without it. Only a
`--no-default-features` build lacks ZGC; there `-XX:+UseZGC` warns and falls
back to Generational, like any unknown collector name.

What ZGC here does **not** have is what the OpenJDK name promises about the
*pause*: **no slot ever holds a colored pointer and no load barrier is ever
armed**, so every collection — mark, sweep and slide alike — stops the world.
Liveness is a bit in the object header, not metadata bits in a pointer. The
barrier and colored-pointer machinery is present in the source and is exercised
only by tests.

It *does* have, today and by default: **compaction** (an arena slide inside the
pause), **thread-local allocation buffers** on both the backend's own path and
through `VmHeap::refill_tlab`'s `Zgc` arm (`gc/src/vm_heap.rs:4171`;
`CRATONVM_ZGC_TLAB=0` turns both off, since they carve from the same source),
and **generations** and **concurrent marking** behind the two opt-ins in the
table. Any page telling you this collector never moves an object, or that
`VmHeap::refill_tlab` returns `None` here, is describing the collector as it
stood before 2026-08-13 and 2026-09-02 respectively.

**Sizing.** This page told operators to *"budget ~1.5x the heap a compacting
collector needs"* until 2026-09-21. **That was wrong** — it was written when the
collector did not compact, and it survived the 2026-08-13 default flip by over
a month, in the one page an operator sizing a production heap would read.
Size a ZGC heap as you would any compacting collector. What does still apply is
the **headroom** caveat, because a cycle may decline to compact (the cost gate
declines it, the JIT coverage proof refuses it, or
`CRATONVM_ZGC_RELOCATE=0`): free memory can then be plentiful and still too
broken up to serve one large array, which is why the arena reserves a floor of
`-Xmx / 8` for the large-object end. See
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
