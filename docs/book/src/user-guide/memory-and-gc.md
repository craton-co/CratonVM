# Memory & Garbage Collection

CratonVM manages the Java heap with high-performance garbage collectors and sizes the heap ergonomically based on host resources. This chapter explains heap operation, collector selection, tuning, and pause diagnostics. For internal implementation details, see [The Garbage Collector](../internals/garbage-collector.md).

## Heap Sizing

### Ergonomic Sizing Default

When `-Xmx` is not specified, CratonVM dynamically computes an optimal maximum heap size:

- **Fraction:** Approximately **¼ of physical RAM** (aligning with OpenJDK's `-XX:MaxRAMPercentage=25`).
- **Floor:** 256 MiB — ergonomic sizing raises the heap allocation above the minimal baseline.
- **Cap:** 32 GiB default ceiling. The heap reserves virtual address space and commits physical memory on demand, so a generous ceiling does not consume unused physical RAM. Override this cap with `CRATONVM_DEFAULT_HEAP_MAX_MB=<MiB>`.
- **Fallback:** If physical system memory cannot be detected, the 256 MiB baseline applies.

Precedence order:
```text
Explicit -Xmx  →  Ergonomic Default (¼ RAM, capped)  →  256 MiB Floor
```

### Explicit Heap Limits

```bash
# Set 2 GB max heap
cratonvm --Xmx 2g --classpath . MyApp

# Set 512 MB max heap
cratonvm --Xmx 512m --classpath . MyApp

# Set initial heap (-Xms) and maximum heap (-Xmx)
cratonvm --Xms 512m --Xmx 2g --classpath . MyApp
```

- `-Xmx` sizes the virtual address reservation.
- `-Xms` specifies the memory committed at VM startup across all collectors. If `-Xms` exceeds `-Xmx`, it is automatically clamped to `-Xmx`.

### Overriding Ergonomics

| Goal | Method |
|------|--------|
| Specify exact heap size | `--Xmx <size>` |
| Cap ergonomic maximum | `CRATONVM_DEFAULT_HEAP_MAX_MB=<MiB>` |
| Disable ergonomics (revert to 256 MB) | `CRATONVM_DEFAULT_HEAP_ERGONOMICS=0` |

Passing `--verbose:gc` logs the calculated heap configuration at startup:

```text
[cratonvm] ergonomic default max heap: 4096 MB (1/4 physical RAM; set -Xmx or CRATONVM_DEFAULT_HEAP_ERGONOMICS=0 to override)
```

## Garbage Collectors

CratonVM provides three collectors tailored to different workload profiles:

| Collector | CLI Flags | Characteristics |
|-----------|-----------|-----------------|
| **ZGC** (Default) | Default, `-XX:+UseZGC`, or `--XX:UseGc ZGC` | **Concurrent low-latency collector (default).** Features arena slide compaction, thread-local allocation buffers (TLAB), concurrent marking (`CRATONVM_ZGC_CONC_START`), and generational splitting (`CRATONVM_ZGC_GENERATIONAL=1`). Designed for minimal pause times and responsive workloads. |
| **Generational** | `-XX:+UseGenerationalGC` or `-XX:-UseZGC` | **High-throughput generational collector.** Two-generation layout (nursery young space and tenured old space), Cheney copying young collector, mark-sweep old collector with selective promotion, write barriers, and card table tracking. |
| **G1** | `-XX:+UseG1GC` or `--XX:UseGc G1` | **Region-based collector.** Divides the heap into equal-sized regions with incremental mixed collection passes. |

### The ZGC Architecture

ZGC is enabled by default in standard builds. Its key architectural components include:

- **Arena Compaction**: Performs compaction passes to eliminate fragmentation. Compaction behavior can be tuned or toggled via `CRATONVM_ZGC_RELOCATE`. Large-object compaction is managed via `CRATONVM_ZGC_HIGH_COMPACTION`.
- **Thread-Local Allocation Buffers (TLAB)**: Fast-path thread-local bump allocation avoids synchronization bottlenecks during object allocation.
- **Concurrent Marking**: Background marking threads trace object graphs concurrently with application threads when enabled (`CRATONVM_ZGC_CONC_START=60`), significantly shrinking pause times.
- **Generational Support**: Optional generational partitioning (`CRATONVM_ZGC_GENERATIONAL=1`) separates short-lived transient objects from tenured data.

### Sizing and Headroom Guidelines

Size a ZGC heap as a compacting collector with sufficient allocation headroom. A reserve floor (`-Xmx / 8`) is dedicated to large-object arrays to prevent fragmentation under bursty allocation loads.

```bash
# Run with default ZGC
cratonvm --classpath . MyApp

# Run with Generational Collector
cratonvm -XX:+UseGenerationalGC --classpath . MyApp

# Run with G1 Collector
cratonvm -XX:+UseG1GC --classpath . MyApp
```

## Monitoring GC Activity

To observe GC cycles, collections, and pause times:

```bash
# Basic console output
cratonvm --verbose:gc --classpath . MyApp

# Structured unified logging (HotSpot compatible)
cratonvm --Xlog "gc*=info:stdout:time,level,tags" --classpath . MyApp
```

## Out of Memory Diagnostics

When heap capacity is exhausted, CratonVM generates an `OutOfMemoryError`. To trigger an automatic heap dump for memory profiling:

```bash
cratonvm -XX:+HeapDumpOnOutOfMemoryError -XX:HeapDumpPath=./oom.hprof \
         --classpath . MyApp
```

The captured `.hprof` file is fully compatible with standard analysis tools such as Eclipse Memory Analyzer (MAT), VisualVM, and YourKit.

## Operational Tuning Advice

- **Frequent pauses under high allocation pressure**: Increase `-Xmx`. Giving the collector extra breathing room reduces collection frequency.
- **Container deployments**: The ergonomic sizer defaults to host RAM. Inside containers with memory limits, always provide explicit `-Xmx` (recommended: 50–75% of container memory) or set `CRATONVM_DEFAULT_HEAP_MAX_MB`. See [Containers & cgroups](containers.md).
- **Stack overflow issues**: If encountering `StackOverflowError` in deep recursive call chains, adjust the Java thread call depth (`RJ_MAX_STACK_DEPTH`) and system thread stack size (`RUST_MIN_STACK`).
