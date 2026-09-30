# Integer and long GPU reductions

GPU offload supports proven integer and long reductions whose method descriptor
returns `)I` or `)J`. The analyzer requires a counted loop, a loop-carried
accumulator that feeds the returned local, array reads, and no array store.
The interpreter receives the downloaded scalar through
`DispatchOutcome::HandledWithValue` in `vm/src/runtime/offload.rs`.

The PTX epilogue uses the one-operand reduction form
`red.global.add[.u64] [ptr], value;`. This matters: the older two-operand
`atom.global.add [ptr], value;` form was rejected by `ptxas`, silently forcing
CPU fallback. `ptxas_round_trip_dot_reduction` guards the emitted form.

Float and double reductions intentionally remain on CPU. Their atomic-add
order is not bit-identical to Java's sequential floating-point accumulation.

See also the historical validation record.

## The warp fold, and the block fold that was measured and rejected

**2026-09-02.** Until this date every thread issued its own
`red.global.add` into the single accumulator cell: a dot product over
2^24 elements was 16.7M atomics to one cache line. Each warp now folds
its 32 lanes with five `shfl.sync.down.b32` steps and lane 0 issues one
atomic — 32x fewer. `GpuDotBench` at 2^24 went from 23 ms to 2 ms with
the checksum unchanged. Exact for `int`/`long`: two's-complement
addition is associative, so reordering the sum changes nothing. Float
and double keep the CPU exclusion described above.

The obvious next step — folding the per-warp partials through shared
memory so the whole block issues ONE atomic, 8x fewer again at a
256-thread block — was implemented the same day and **measured slower**:

| bench (RTX 2060, 2^26, interleaved) | warp fold only | + block fold |
|---|---|---|
| `BlockReduceBench` best_ms (min arithmetic) | 2.25-2.62 | 2.74-2.90 |
| `GpuDotBench` dot_ms (compute-bound) | 8, 8, 14 | 9, 15, 15 |

The warp-only arm won all four rounds of the first and two of three of
the second. `red.global.add` returns nothing, so a warp issues it and
retires; a `bar.sync` makes every warp in the block wait for the slowest
at the end of the kernel, and that costs more than the seven atomics it
saves. `BlockReduceBench` exists to make this measurable at all —
`GpuDotBench` does 64 adds per element and reports whole milliseconds,
so both arms read `dot_ms=2` at 2^24 and the question could not be
asked of it.

The barrier also forced the bounds-check deopt to stop `ret`-ing from
the middle of the kernel: one thread leaving while its block waits at
`bar.sync` is a HANG rather than a wrong answer. That constraint is gone
with the stage, but `test_classes/gpu/BoundsDeoptReduction.java` — a
reduction that fails its bounds check — was written for it and is kept.

Worth re-measuring only on hardware whose global atomics are much slower
than sm_75's.

