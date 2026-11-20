# VM Benchmark Baselines

`baseline.json` is the CratonVM Criterion baseline consumed by
`bench-gate`. `hotspot-baseline.json` is the HotSpot comparison baseline
consumed by `bench-hotspot-compare`.

The checked-in files are placeholders: every metric is present with
`median_ns: 0`. That state is intentional and means "bootstrap, not yet
captured". `bench-gate` treats zero values as `BOOTSTRAP`, but a missing or
malformed baseline file is an error. Refresh baselines only from a deliberate
benchmark capture using the gate tools' update/capture flows.

Historical Criterion output under `target_bench/` is generated data. It is
kept out of source packages through `vm/Cargo.toml` and should not be used as
the committed gate baseline.
