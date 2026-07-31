# Committed heap exceeds `-Xmx`: the young semi-space expands 4x with no total-heap budget

## Status
**OPEN** — found 2026-07-31 while root-causing
`bug-h2-testoutofmemory-sigabrt-young-old-gen-both-exhausted` (now FIXED and
retired to `docs/internal/fixed-suite-bugs/h2-suite-bugs/`). Filed separately
because the fix is a GC-wide sizing change with a far larger blast radius than
that record.

## Severity
**HIGH (resource correctness).** `-Xmx N` does not bound the process. Measured:
a `--Xmx 1g` CratonVM reached **4.0 GB RSS / 7.5 GB virtual** and was killed by
the Linux OOM killer (`oom-kill: ... task=cratonvm-oom-fi, anon-rss:4054336kB`)
instead of throwing a Java `OutOfMemoryError`. On a container with a memory
limit — or a shared build host — that is an unrecoverable, un-catchable death
where HotSpot would have thrown.

## Mechanism

`GenerationalHeap::with_capacity(total)` splits the budget correctly:

* young semi-space = `total / 4` each (from + to = `total / 2`),
* old gen = `total / 2`.

`GenerationalHeap::with_sizes` then computes the expansion ceiling **from the
young semi alone**:

```rust
let max_young = young_semi_size.max(1024).saturating_mul(MAX_HEAP_EXPANSION_FACTOR);
```

`MAX_HEAP_EXPANSION_FACTOR` is 4 (`gc/src/gen_heap.rs`), so after enough
low-reclamation minor GCs each semi-space may reach `total`. Worst case
committed:

| | `--Xmx 1g` |
|---|---|
| young from (initial -> max) | 256 MB -> 1024 MB |
| young to (initial -> max) | 256 MB -> 1024 MB |
| old gen (fixed) | 512 MB |
| **total** | **1 GB -> 2.5 GB** |

Observed mid-run at `--Xmx 1g` (from the improved fatal-OOM diagnostic):
`young_from` 512 MB, `young_to` **1024 MB**, `old_gen` 512 MB — 2 GB of Java
heap for a 1 GB `-Xmx`, and 4 GB RSS once metaspace/JIT/native buffers are
counted.

## Why it is not a one-line fix

The old generation is a single fixed-capacity `Vec` (`gc/src/old_gen.rs`) with
raw pointers into it, so it **cannot grow** — young expansion is currently the
collector's only adaptive headroom. Simply clamping the ceiling to
`(max_heap - old_capacity) / 2` equals the initial semi under the 50/50 split,
i.e. it disables young expansion outright, and that headroom is load-bearing:
see the anti-livelock note on `young_trigger_floor` (spring-webflux
`RequestMappingMessageConversionIntegrationTests` wedged at live=511 MB against
a 512 MB trigger). Shrinking old gen to make room instead would hurt exactly the
promotion-heavy workloads that need it — the H2 case promoted 699 MB into a
512 MB old gen.

## Candidate directions
1. Segmented / chained old generation so old gen can grow, then enforce
   `2 * young_semi + old_committed <= Xmx` as a single shared budget.
2. Keep the 4x ceiling but treat `-Xmx` as the hard cap and start the split
   smaller (e.g. young semi `Xmx/8`), accepting the measured large-`-Xmx`
   regression from a smaller initial semi
   (`docs/internal/performance/binarytrees-bt18-half-gap-20260730.md`).
3. At minimum: report the real committed ceiling at startup and in the
   fatal-OOM diagnostic so `-Xmx` is not silently misleading.

## Reproduce
```bash
<cratonvm-bin> --java-home <jdk25> --nojit --Xmx 1g \
  -c "<h2 test classpath>" org.h2.test.db.TestOutOfMemory &
watch -n5 'ps -o rss= -C cratonvm'    # climbs past 4 GB
```
Any allocation-heavy workload that survives several low-reclamation minor GCs
shows it; the improved `FATAL-OOM detail:` line prints both semi-space
capacities directly when the heap finally fills.
