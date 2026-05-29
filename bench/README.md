# Benchmark Gate (NEW-20)

The `bench-gate` binary in `vm/src/bin/bench_gate.rs` enforces
performance regressions across releases.

## How it works

1. `cargo bench --bench vm_benchmarks` runs the criterion suite in
   `vm/benches/vm_benchmarks.rs` and writes `estimates.json` files
   under `target/criterion/`.
2. `cargo run --release --bin bench-gate` parses every
   `target/criterion/<bench>/<id>/new/estimates.json`, compares each
   metric's median against `bench/baseline.json`, and exits non-zero
   if any metric has regressed by more than the configured threshold
   (default: 15%, NEW-20.2).

## Bootstrapping a new baseline

The committed `baseline.json` ships with all metrics set to `0`. A
zero baseline is treated as "uninitialized" by the gate, so the
first run on a new host passes cleanly while reporting `BOOTSTRAP`
for every metric.

To capture an actual baseline on your own hardware:

```bash
cargo bench --bench vm_benchmarks
cargo run --release --bin bench-gate -- --update-baseline
```

The gate will write the measured medians back to `bench/baseline.json`.
**Do not commit a baseline captured on developer hardware to the
shared repository**; only the CI runner's measurements should be the
source of truth.

## CI integration

`.github/workflows/bench-gate.yml` runs the gate on every push to
`main` and on PRs touching `vm/`, `jit/`, `gc/`, or `native-builtins/`.
The workflow uploads `bench/last_run.json` as an artifact.

## Adding a benchmark

1. Add a `bench_*` function to `vm/benches/vm_benchmarks.rs` and
   reference it in the `criterion_group!` macro.
2. The first run will report it as `NEW` (does not fail the gate).
3. Once the metric is stable, add an entry to `bench/baseline.json`
   with `"median_ns": 0` to opt into bootstrap-then-enforce.

## Tuning the threshold

```bash
bench-gate --threshold 0.10   # tighter: fail on > 10% regression
bench-gate --threshold 0.20   # looser: fail on > 20% regression
```

The geometric mean of all per-metric ratios is also gated against
the threshold, so a wide-but-shallow regression across many metrics
still trips the gate.

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | Gate passed (every metric within threshold or bootstrapping) |
| 1    | At least one regression or required metric missing from run |
| 2    | Baseline file missing/corrupt and `--update-baseline` not given |
| 3    | No criterion data found — run `cargo bench` first |
| 4    | Invalid CLI arguments |

## License

Apache-2.0. See `../LICENSE` and `../NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
