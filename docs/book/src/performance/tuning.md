# Performance Tuning

Tune CratonVM by measurement, starting with correctness and the largest
end-to-end cost. This chapter covers workload tuning; historical results and
the benchmark protocol are in [Benchmarks](benchmarks.md) and the repository
`BENCHMARK.md`.

## The non-negotiable baseline

Before changing a performance setting:

1. make the workload deterministic or give it a deterministic checksum;
2. run the same classes and inputs on HotSpot;
3. verify CratonVM JIT, CratonVM `--nojit`, and HotSpot agree;
4. build the comparison binaries from recorded commits;
5. use the same release profile for both; and
6. record host load, CPU affinity, memory limit, JDK, and every sample.

A faster mismatching checksum is a correctness bug.

The default release profile uses fat LTO and one codegen unit:

```bash
cargo build --release -p cratonvm-cli --bin cratonvm
```

An LTO-disabled build is useful when memory-constrained development hosts
cannot link the default profile, but its absolute time is not comparable with a
fat-LTO baseline. It is valid for an A/B only when both sides use the identical
profile.

## Tune in this order

### 1. Remove environmental noise

- Pin a CPU for single-threaded microbenchmarks.
- Run in a quiet window.
- Avoid concurrent compilers and VM suites.
- Fix CPU frequency policy if the environment allows it.
- Give HotSpot and CratonVM the same heap and application flags.
- Alternate the two VMs rather than running all samples from one first.

On a shared host, report ratios and load with the absolute values. If the
machine is saturated, use the run for correctness and diagnosis, not for a
durable performance claim.

### 2. Size memory explicitly

Set `-Xmx` large enough for the live set and allocation bursts, while leaving
native-memory headroom. Symptoms of an undersized heap include frequent
collections, allocation stalls, and a live set that nearly fills the heap after
each cycle.

Larger is not automatically faster: an oversized eagerly committed heap
increases process memory and can trigger container pressure. Measure collection
frequency, pause time, and RSS together.

### 3. Establish the compiler contribution

Compare:

```bash
cratonvm ...
cratonvm --nojit ...
```

If JIT is materially faster and correct, focus on hot compiled methods. If the
gap is small, the workload may be dominated by native/library work, allocation,
class loading, I/O, or methods that stay interpreted.

`CRATONVM_JIT_THRESHOLD` controls method hotness eligibility, and
`CRATONVM_JIT_OSR=0` disables back-edge OSR for diagnosis. Lowering the
threshold can help a short benchmark but may increase compile time and code
cache pressure in an application. Do not publish a tuned threshold without
including startup and steady-state effects.

Bound the code cache with `CRATONVM_JIT_CODE_CACHE_MAX_MB` only when there is a
demonstrated memory requirement. Once full, new methods remain interpreted, so
an undersized cache can create a gradual throughput decline.

### 4. Select a collector from evidence

The default generational collector is the general-purpose baseline. Evaluate
G1 or moving-young modes only with the application's allocation profile and
latency objective.

| Symptom | Evidence to collect | Possible direction |
|---------|---------------------|--------------------|
| High allocation rate, low live set | Young-cycle frequency and allocation throughput | Increase heap/young capacity; reduce transient allocation. |
| Large retained live set | Post-GC occupancy and old-generation growth | Increase capacity or reduce retention. |
| Fragmentation/footprint pressure | Allocation failures despite reclaimable space | Evaluate moving young on the exact workload. |
| Long experimental-G1 pauses | Region occupancy and default-collector comparison | Return to default or tune only with repeatable evidence. |

The moving path is fail-closed: when the VM cannot prove complete relocation
coverage it diverts to the non-moving cycle. That protects correctness but
means an opt-in setting does not guarantee every cycle moves objects.

### 5. Profile the actual hot path

Use OS profiling and JFR to determine whether time is spent in:

- interpreted dispatch;
- JIT-compiled application code;
- compiled slow-path helpers;
- allocation and GC;
- monitor contention or parking;
- class loading and initialization;
- native methods;
- I/O; or
- host scheduling.

Optimize the dominant category. A micro-optimization in the hashed dispatch
stub will not improve a workload spending most of its time in regex natives or
old-generation collection.

## Understanding the current fast paths

### Allocation

Baseline and optimizing x86-64 JIT tiers share the same object-allocation
lowering contract. The common case is a thread-local allocation-buffer bump;
class initialization, TLAB refill, and failure enter the runtime slow path.
Escaping C2 allocations no longer force a whole-method fallback.

Application guidance remains conventional: reduce avoidable transient
allocation, reuse buffers where ownership is clear, and measure object
lifetime. Do not pool small objects blindly; pooling can enlarge the live set
and cost more than TLAB allocation.

### Virtual and interface dispatch

Compiled sites progress from a monomorphic cache through a small PIC. After
that, a compact eight-set/two-way hashed vtable tail is probed without a
mutex-protected map. Highly polymorphic sites still pay more than stable sites.

Use profiles before redesigning application types. Interface abstraction is
usually worth keeping; only a proven, dominant, highly megamorphic site merits
specialization.

### Monitors

Uncontended compiled `monitorenter`/`monitorexit` use the VM's thin-lock path.
Contention enters the inflated/parking path. If monitor time dominates, reduce
the critical section or ownership contention rather than attempting to tune a
VM flag.

### Native calls

The common x86-64 Java argument envelope stays inline through eight slots
during decoding, forwarding, and pin-index preparation. Larger descriptors
fall back to heap-backed scratch storage. If a native boundary dominates,
batching useful work per call is normally more valuable than changing a method
signature solely to stay under the inline capacity.

## A/B run template

```text
host:
cpu / affinity:
host load:
memory / cgroup:
JDK:
commit A / binary SHA-256:
commit B / binary SHA-256:
build profile:
VM flags:
workload and input:
expected checksum:
sample order:
all samples:
median A:
median B:
profile delta:
conclusion:
```

Keep raw result files beside the evidence document. If the host is too loaded
for a verdict, say so; correctness results can still be valid.

## Avoid these tuning mistakes

- Comparing a debug build with an optimized JVM.
- Comparing LTO-off absolute time with a fat-LTO baseline.
- Changing the JDK or application between A and B.
- Reporting the best sample instead of the median and full sample set.
- Dropping slow samples without a predeclared rule.
- Raising/lowering the JIT threshold before confirming which tier ran.
- Treating `--nojit` as a production optimization.
- Disabling verification for speed.
- Re-anchoring a baseline after a regression without root cause and evidence.
