# Benchmarks

Use the maintained [benchmark guide](../../../../BENCHMARK.md),
[methodology](../../../benchmarking/methodology.md), and
[reliability gate](../../../benchmarking/reliability-gate.md).

CPU workloads live in `tools/bench/CratonBench.java`. Framework and GPU harnesses
live alongside it under `tools/bench/`. Compare against HotSpot on the same
machine with matching JDK, heap, collector, workload, and execution-policy flags.
Report checksums, repeated samples, and variation together. A CPU fallback is
not a GPU result, and a microbenchmark does not establish framework throughput.

Criterion baselines under `vm/bench/` are gate inputs. Zero-valued bootstrap
entries are placeholders, not measured performance claims.

Published measurements are in the [CPU tables](../../../../BENCHMARK.md#cpu-benchmark-results),
[cold-start tables](../../../../BENCHMARK.md#cold-start-and-framework-workloads),
[GPU tables](../../../../BENCHMARK.md#gpu-benchmarks), and
[application tables](../../../../performance.md). CPU ratios are CratonVM time
divided by HotSpot time: below 1 is faster, above 1 is slower. Keep the hardware,
collector, execution policy, and workload size with each result.
