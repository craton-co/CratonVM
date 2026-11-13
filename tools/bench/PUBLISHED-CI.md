# Published HotSpot comparison

Run the manually dispatched `Published HotSpot comparison` workflow on the
dedicated Linux benchmark runner. It builds `cratonvm-cli` from the checked-out
commit with the release profile, then saves all raw logs as an Actions artifact.
There is no fixed performance threshold: a failed process, timeout, checksum
mismatch, or cold-start output mismatch fails the job; timing ratios are evidence
for review.

The CPU runner covers the seven size tables and the tiled matrix table in
`BENCHMARK.md`, under both G1 and ZGC. Each row runs in fresh processes, with
HotSpot and both CratonVM modes interleaved three times on one CPU. The cold
runner covers the seven programs in the same file, with the boot image disabled
to measure cold start. It records user instructions, task-clock, peak RSS and
wall time, and compares stdout byte for byte with HotSpot. The
existing `tools/probes/matrix-hashmap-simd/run.py` covers the experimental
matrix and flat-map table. The 16/32/64 tile calibration compiles each constant
into an otherwise identical benchmark class. GPU rows are excluded because
this runner has no GPU.

The application runner covers the first table in `PERFORMANCE.MD`, including
the H2 TCP case and the four Netty buffer kinds under both Unsafe policies.
It requires the application fixtures already built under `apps_root`, the
Tomcat and Commons Math classpath files under `/root`, and the Hibernate
runner under `/root/hibs/runner`. It compiles its small drivers before timing.
These fixtures are larger than this workflow; if one is missing, preparation
fails instead of publishing a partial comparison. Application rows use one
fresh process per HotSpot and CratonVM arm; the Netty buffer probe has its own
four-round methodology and checksums.

For a focused CPU or cold-start diagnosis on the same host:

```bash
python3 tools/bench/run-published-ci.py \
  --vm "$PWD/target/release/cratonvm" --jdk "$JAVA_HOME" \
  --output /tmp/published-fib --suite cpu --case fib-40-G1GC --reps 3 --cpu 2
```

Result directories are never reused. The JSON contains the source and binary
identity, commands, load, raw samples, median times, and CratonVM/HotSpot
ratios. Shared-host timings are diagnostic when load is high.
