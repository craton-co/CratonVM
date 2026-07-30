# CratonBench Matrix half-time closure (2026-07-30)

Status: fixed.

## Goal and acceptance

The goal was to reduce the elapsed time of the isolated `CratonBench matrix`
row by at least 50%, while retaining its exact checksum `173943680`.

Final acceptance used `bench/CratonBench.java`, Temurin 25.0.3, `-Xmx8g`, and
fresh CratonVM processes on Windows 11. The control and candidate were the
same release binary built from merge commit `d959bbde1`, which contains current
`origin/dev` `e88f14830`; the control set only
`CRATONVM_JIT_MATRIX_DOT=0`. Runs alternated control/candidate order, and no
sample was discarded:

| Variant | Five reported times (ms) | Median |
|---|---|---:|
| Matrix-dot lowering disabled | 9,407, 11,182, 12,291, 9,844, 9,800 | 9,844 |
| Matrix-dot lowering enabled | 3,541, 3,545, 4,137, 3,307, 4,836 | 3,545 |

The enabled median is **2.78x faster** and **63.99% lower** than the disabled
median, clearing the required 50% reduction. All ten executions produced the
exact checksum.

The host was shared with many concurrent release builds, which explains the
wide absolute-time spread. Same-binary A/B, alternating order, five samples,
and a median gate isolate the optimization despite that load.

## Root cause

`CratonBench.matmul` compiles the conventional inner dot product:

```java
for (int k = 0; k < n; k++) {
    sum += a[i][k] * b[k][j];
}
```

The generic bytecode emitter materialized the operand-stack operations and
repeated array plumbing for all 2,097,152,000 multiply-adds. Existing LICM and
bounds-check elimination removed some checks but could not fuse the
`aaload`/`iaload` chain through Java's independently replaceable `int[][]`
rows. The residual address generation, local traffic, and induction
dependencies dominated Matrix.

## Fix

- Recognize only the complete, side-effect-free bytecode loop shape, including
  its accumulator store, unit induction update, and exact back edge.
- Emit a fall-through-only guarded preheader. It resolves invariant state,
  validates the selected A row and B outer range, and keeps the original Java
  local homes untouched until the entire dot product succeeds.
- Keep the A row, B outer array, column, bound, induction variable, and wrapping
  accumulator in reserved registers inside the tight loop.
- Preserve per-row B null and column-bounds checks. Any failed guard branches
  to the original scalar bytecode with its original `k` and `sum`, so null,
  jagged, short, negative-index, and zero-trip behavior remains exact.
- Process eight elements per batch with fixed `k+0` through `k+7`
  displacements, one induction update, and one batch branch. Integer
  multiply-add order and Java two's-complement wrapping are unchanged.
- Support both wide references and the opt-in compressed-oop array layout.
- Provide `CRATONVM_JIT_MATRIX_DOT=0` as a diagnostic kill switch and report
  recognized headers under `CRATONVM_DBG_JIT_GEN`.

The first scalar tight-loop version improved the paired control from
12,021 ms to 6,139 ms but narrowly missed the strict half-time gate. A
four-way version reached medians of 6,902 ms disabled and 3,591 ms enabled
(47.97% lower), which was also rejected. Fixed-displacement eight-way batches
closed the residual gap.

## Validation

- Final release binary:
  `cratonvm-matrix-candidate-merged-e88-019fb302.exe`,
  SHA-256
  `235e511a37e5dcc9639cebdf4bfc42e17b05a2e81c04d524671562e6fb1f973c`.
- The recognition trace reported
  `[JIT_GEN] matrix-dot headers=[31]` for the real
  `CratonBench.matmul` bytecode, with no code-buffer bailout.
- `MatrixDotProbe` passed under default JIT, `--nojit`, the lowering disabled,
  and compressed oops enabled. It covers a normal result, signed-int overflow,
  a zero-trip loop with null arrays, short A/B ranges, negative columns, null
  rows, and failures partway through an eight-element batch.
- `cargo check -p cratonvm-jit --lib`: passed.
- Focused detector test: 1 passed, 0 failed.
- Header-offset inventory contract: 1 passed, 0 failed. The new matrix-dot
  sites are recorded in
  `docs/internal/arch-2026-07-26/x64-flag-skew-and-contracts.md`.
- The full `cratonvm-jit` library process is baseline-blocked on Windows by
  executable-code tests that terminate the process with
  `STATUS_ACCESS_VIOLATION`. The first unskipped run stopped at
  `cooperative_poll_runs_in_a_pure_compiled_method`; after skipping the poll
  and monitor crashes it stopped at `s31_inline_bipush_sipush`. Each of
  `cooperative_poll_runs_in_a_pure_compiled_method`,
  `live_monitor_ops_execute_direct_runtime_stubs`, and
  `s31_inline_bipush_sipush` was rerun alone in a clean detached
  `origin/dev` `276cac509` worktree and reproduced the same access violation.
- `git diff --check`: passed.

Repository-wide formatting remains outside this focused gate because current
`dev` has unrelated pre-existing rustfmt drift in `jit/src/x64.rs`; the
task-specific diff is whitespace-clean.
