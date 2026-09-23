# Arithmetic intensity, and why `--gpu-min-work` should not scale with element width

**Measured 2026-09-04.** Windows 11, CUDA 13.3, RTX 2060 (sm_75),
`cratonvm-cli --features gpu-driver` at `origin/dev`. Harness:
`bench-gpu/intensity-sweep.sh`. Fixture:
`test_classes/gpu/GpuIntensitySweep.java`.

## The question

The 2026-09-02 pricing run measured **one** kernel shape — a single
multiply-subtract per element — and found `long[]` a consistent ~1.55x
LOSS in cold mode (0.59–0.65x over five runs) while `byte[]` won
2.75–3.33x. The natural reading was that `--gpu-min-work` counts
ELEMENTS, so an 8-byte-per-element kernel is admitted on the same terms
as a 1-byte one, and should be held to a higher bar.

That reading could not be checked from that measurement. One kernel at
minimal arithmetic intensity is the worst case *by construction*: it is
where transfer dominates most, so of course the widest type looks worst
there. The open question was where the crossover actually sits, and
whether it moves with width.

So this sweeps INTENSITY as well as width. Every kernel moves the same
bytes; only the ops-per-element change.

## Result

`--gpu` vs no flag, **same binary** (a within-binary A/B — nothing
differs but the flag). n = 2^20, 50 timed iterations after 20 warm-up,
median of 3 rounds with the arm order alternating. Cold mode: the input
is mutated between calls, so every submit pays H2D + D2H.

Speedup = `cpu_us / gpu_us`; above 1.00 the GPU wins.

| type | bytes/elem | ops=1 | ops=4 | ops=16 |
| --- | ---: | ---: | ---: | ---: |
| `byte[]`   | 1 | **4.78x** | 7.85x | 22.07x |
| `int[]`    | 4 | **1.94x** | 3.97x |  9.96x |
| `long[]`   | 8 | **1.04x** | 1.87x |  5.60x |
| `double[]` | 8 | **1.00x** | 2.21x | 13.74x |

Every one of the 12 cells engaged: each GPU run reported
`chunked writeback: taken=70`, matching its 70 calls exactly, and every
`sink` checksum matched its CPU arm. A cell that had silently fallen
back would have been printed as `NOT-ENGAGED` with its ratio suppressed
— that check exists because a fallback arm and a GPU arm run identical
code, and their agreement means nothing.

## What it says

**1. The GPU never loses.** The worst cell in the grid is 1.00x. The
premise for a width-scaled threshold was that wide types LOSE at low
intensity; here `long[]` at one op is 1.04x and `double[]` is 1.00x —
parity, not loss. The 2026-09-02 finding of 0.59–0.65x does not
reproduce. Much has landed since (narrow-width chunking, the residency
and submission work, the gate narrowing), so the most likely explanation
is simply that the measurement is stale; this was not chased further.

**2. Width changes the MAGNITUDE, not the crossover.** At one op the
ordering is `byte` 4.78x > `int` 1.94x > `long` 1.04x ≈ `double` 1.00x,
which is exactly bytes-per-element (1 < 4 < 8 = 8). The width effect is
real. But every width is already at or above break-even at the *lowest*
intensity tested, so there is no width-dependent crossover to encode.

**3. The GPU is transfer-bound here, and that is the whole story.**
GPU time barely moves with arithmetic while CPU time scales with it:

| type | gpu@1 | gpu@4 | gpu@16 | GPU spread | CPU 1→16 |
| --- | ---: | ---: | ---: | ---: | ---: |
| `int[]`    |  926 |  893 |  932 |  4% |  5.2x |
| `long[]`   | 1786 | 2047 | 1839 | 15% |  5.5x |
| `double[]` | 2471 | 2484 | 2283 |  9% | 12.6x |
| `byte[]`   |  498 |  472 |  476 |  6% |  4.4x |

Sixteen times the arithmetic costs the GPU nothing measurable. And at
one op, GPU time tracks bytes moved — 498 us (1B), 926 us (4B),
1786/2471 us (8B). So:

    gpu_cost  ~=  fixed overhead + bytes moved
    cpu_cost  ~=  ops x elements

which is why the speedup column grows roughly linearly with ops, and why
wide types look worst exactly where arithmetic is scarcest.

## Superseded in part, same day

The recommendation below was drawn at a single size, n = 2^20, where
every width is at or above break-even. Sweeping `n` instead
(`docs/gpu/offload-crossover-and-min-work-20260904.md`) found the
break-even ranges from ~32,000 elements for `byte[]` to over 1,048,576
for `long[]` -- a ~32x spread, monotonic in bytes per element.

So **"do not scale by width" is too strong**: it holds at n = 2^20 and
fails near the threshold, which is the only place an admission decision
is actually made. The 2026-09-02 instinct that wide types need a higher
bar was right; this document sampled 256x above the boundary and so
could not see it. It says as much below -- "nothing here is a loss, so
nothing here locates a refusal boundary" -- which is what prompted the
follow-up.

Everything else here stands: the transfer-bound model, the flat GPU time
across intensity, and the fact that `long[]` at 0.59-0.65x does not
reproduce at this size.

## Recommendation (superseded -- see above)

**Do not scale `--gpu-min-work` by element width.** The change would
refuse wide-type work that is, at worst, break-even — a pessimisation
justified by a measurement that no longer reproduces.

The axis that actually decides the answer is **arithmetic intensity**.
An admission test worth building would estimate ops-per-element in the
analyzer and weigh it against bytes moved, rather than counting elements
and correcting for width. Note that this grid gives no evidence for
where such a threshold belongs: nothing here is a loss, so nothing here
locates a refusal boundary. Finding one needs a shape that actually
loses — smaller `n`, or a kernel whose output is larger than its input.

## Limits

* One size (n = 2^20) and one device. The crossover is a function of
  transfer cost, so it will move with both.
* Cold mode only. In hot mode the residency cache serves the input and
  the transfer half of the model largely disappears — the GPU should
  win by more, not less, so this is the conservative direction.
* No control arm. The large ratios (>= 1.87x) are far outside the ~10–17%
  round-to-round noise measured on this box the same day
  (see `docs/gpu/cuda-oxide-evaluation.md`), but the two ops=1 cells at
  1.00x and 1.04x are **inside** it: read those as parity, not as a
  small win.
* Under `--gpu` the JIT gate denies the `run` dispatcher
  (`calls-eligible-kernel`), so the GPU arm pays interpreted dispatch the
  CPU arm does not. That is a constant adder working *against* the GPU in
  every cell, so it cannot have manufactured these wins.
