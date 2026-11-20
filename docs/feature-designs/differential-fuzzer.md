# Semantic differential fuzzer vs HotSpot

**Status:** Shipped (default on, and blocking in CI).

## What it does today

`difftest/` is a workspace member, `cratonvm-difftest`, whose installed binary
carries the same name (the repository requires unique binary names). It
generates and mutates Java programs, runs them on both CratonVM and a real
JDK, and diffs observable behaviour — stdout/stderr, thrown exception type and
message, return value, exit code — with corpus growth, crash minimization and
a divergence ledger.

Modules: `generate.rs`, `mutate.rs`, `minimize.rs`, `oracle.rs`, `ledger.rs`,
`matrix.rs`, `crossmode.rs`, `census.rs`.

**The CI gate is blocking, not advisory.** The `difftest-gate` job runs
`gate --corpus difftest/seeds` against a real JDK 25 oracle without
`continue-on-error`. A second gate runs `--corpus difftest/seeds-jdk-only`.
The opcode `matrix` step is explicitly report-only.

The panic-only parser fuzzers are a separate harness with a separate posture —
see [`fuzzing-state.md`](fuzzing-state.md).

**JIT execution paths are gated against the interpreter, not HotSpot.** Two
further subcommands use `nojit` as the oracle, so they need no HotSpot run and
no ledger rows (operator guide:
[`docs/testing/jit-differential.md`](../testing/jit-differential.md)):

* `path-gate` runs a corpus through `Mode::execution_paths()` plus `moving-gc`
  and fails on any reproducible disagreement with `nojit` that is not listed in
  `difftest/path-gate-known.json`. The `difftest-jit-paths` CI job runs it over
  `difftest/seeds` with the IR verifier lanes exported, and leaves the existing
  `gate` step byte-identical.
* `fuzz-jit` is the bytecode tier §3.1 describes, with a differential oracle
  instead of a panic oracle. It writes verifiable class files directly through
  a typed assembler (`classgen.rs`), covers sixteen JIT-hostile categories
  (`MIN_VALUE / -1`, shift masking, NaN compares, float-to-int saturation,
  switch edge keys, wrapping loop bounds, evaluation-order-sensitive `double`
  sums, aliased arrays, every `dup*_x*` form, `wide`), and minimizes any split
  to a runnable `.class`. `jit-differential-nightly.yml` runs a fixed seed range
  under default, GC-stress, eager-deopt and major-49 axes.
