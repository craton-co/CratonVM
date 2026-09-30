# GPU feature wave — hardware validation (2026-07-11 evening)

RTX 2060 (sm_75), driver 591.86, `--features gpu-driver`, JDK 25 HotSpot as
reference. All checksums/samples bit-exact vs HotSpot unless noted. Box had
~25-30% background CPU load (see morning results files for the caveat).

## Reduction dispatch (new)

`GpuDotBench.dotReduce(int[], int[]) -> long` (`sum += (long) a[i] * b[i]`),
warm best-of-5, full H2D + kernel (single-cell `red.global.add.u64`) + D2H:

| N | CratonVM CPU | **CratonVM GPU** | HotSpot C2 | checksum |
|---|---|---|---|---|
| 2²⁴ | 76 ms | **18 ms** | 7 ms | ✓ exact, incl. vs in-process oracle |

Honest framing: PCIe-bound + accumulator contention — beats CratonVM's own CPU
4.2×, does not beat vectorized C2 at this size. The feature completes the
transparent-offload surface; TornadoVM 4.0.1 PTX throws
`TornadoInternalError: unimplemented` on the equivalent `@Reduce` over
`LongArray` (`bench-tornado/TornadoDotBench.java`), so this shape is a
CratonVM-only capability between the two.

Found+fixed during validation: the reduction epilogue emitted 2-operand
`atom.global.add` — invalid PTX (`ptxas`: "Arguments mismatch for instruction
'atom'"), so every reduction had silently blacklisted to CPU since the
epilogue was written. Now `red.global.add`; ptxas round-trips cover all six
lowering shapes.

## ldc-constant kernels (new)

`GpuLdcBench` — 96-MAD chain with constants outside sipush range (forces
`ldc`), warm best-of-5:

| N | CratonVM GPU | HotSpot C2 (AVX2) | note |
|---|---|---|---|
| 2²⁴ | **8 ms** | 7 ms | SAMPLE bit-exact; was analyzer-ineligible (CPU-bound ~2,000 ms class) before ldc support |

## JIT-caller gate (new)

`GpuWarm f 4194304 100` (100 hot reps — enough for the caller loop to cross
OSR thresholds): warm best stays **2 ms** through all 100 iterations. Without
the gate, caller OSR silently moved dispatch into JIT code mid-run and every
subsequent call ran on CPU (~500 ms class at this size).

## Regression gates (all pass)

- `GpuProbe` 2²⁴: MAP/DOT/MAX checksums all match HotSpot.
- `BoundsDeopt2 --gpu --nojit`: GPU bounds trip → deopt → Java-correct AIOOBE.
- `--print-gpu-decisions` now prints analyzer verdicts without `RUST_LOG`
  (used to diagnose the invalid-PTX blacklisting live).
- jit-cuda: 109 tests incl. 6 ptxas round-trips; vm stub integration: 18 tests;
  native-builtins craton_gpu: 43 tests.
