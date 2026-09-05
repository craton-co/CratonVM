# `--gpu-min-work` is ~8-256x too low, and the break-even IS width-dependent

**Measured 2026-09-04.** Windows 11, CUDA 13.3, RTX 2060 (sm_75),
`cratonvm-cli --features gpu-driver` at `origin/dev`. Harness:
`bench-gpu/crossover-n.sh`. Fixture:
`test_classes/gpu/GpuIntensitySweep.java`.

## Headline

`--gpu-min-work` defaults to **4096 elements**. At that size the GPU is
**3-5x SLOWER than the CPU**, and the VM dispatches anyway. Verified at
the default setting, not just with the threshold disabled:

| type | n | cpu ns/call | gpu ns/call | dispatches | speedup |
| --- | ---: | ---: | ---: | ---: | ---: |
| `int[]`  | 4096 | 16316 | 64599 | 219/219 | **0.25x** |
| `byte[]` | 4096 | 15153 | 63559 | 219/219 | **0.24x** |
| `int[]`  | 8192 | 13697 | 70823 | 219/219 | **0.19x** |
| `byte[]` | 8192 | 21722 | 65947 | 219/219 | **0.33x** |

Fully engaged — 219 dispatches for 219 calls. This is not a fallback
being mismeasured; it is the offload path winning admission and losing
the race.

## The curve

`--gpu` vs no flag in the same binary, `--gpu-min-work 1` so nothing is
refused on size, ops=1 (lowest intensity, the worst case for the GPU and
so the conservative place to site a threshold), 200 timed iterations
after 20 warm-up, median of 3 rounds with the arm order alternating.

Speedup = `cpu_ns / gpu_ns`; below 1.00 the GPU loses.

| n | `byte[]` 1B | `int[]` 4B | `double[]` 8B | `long[]` 8B |
| ---: | ---: | ---: | ---: | ---: |
| 1048576 | 4.94x | 1.90x | 1.05x | 0.93x |
| 262144  | 4.03x | 1.32x | 0.90x | 0.79x |
| 65536   | 1.86x | 0.90x | 0.61x | 0.55x |
| 16384   | 0.48x | 0.32x | 0.30x | 0.27x |
| 8192    | 0.31x | 0.27x | 0.16x | 0.18x |
| **4096**| **0.26x** | **0.20x** | **0.21x** | **0.21x** |
| 2048    | 0.21x | 0.10x | 0.15x | 0.09x |
| 1024    | 0.13x | 0.07x | 0.07x | 0.08x |

Break-even (speedup crosses 1.00), read off the curve:

| type | bytes/elem | break-even n | break-even bytes |
| --- | ---: | ---: | ---: |
| `byte[]`   | 1 | ~32,000    | ~32 KB |
| `int[]`    | 4 | ~65-130,000 | ~256-512 KB |
| `double[]` | 8 | ~250K-1M   | ~2-8 MB |
| `long[]`   | 8 | > 1,048,576 | > 8 MB |

The current default sits at 4096 — between **8x** too low for `byte[]`
and **256x** too low for `long[]`.

## This corrects the sibling document

`docs/gpu/arithmetic-intensity-sweep-20260904.md`, written earlier the
same day, concluded "do not scale `--gpu-min-work` by element width".
That conclusion was drawn at a single size, n=2^20, where every width is
at or above break-even — and it is too strong.

Both measurements are right about what they measured:

* **At n=2^20** no width loses, so nothing there argues for a
  width-dependent threshold. That much stands.
* **Near the threshold**, where an admission decision is actually made,
  break-even ranges from ~32K elements for `byte[]` to >1M for `long[]`.
  That is a **~32x spread**, and it is monotonic in bytes per element.

So the 2026-09-02 instinct — that a wide type should be held to a higher
bar — was **right**, and the earlier "no width scaling" reading was an
artifact of sampling one size 256x above the threshold. The 09-02 run's
specific claim, `long[]` at 0.59-0.65x, still does not reproduce at
n=2^20 (0.93-1.04x across three runs); but its conclusion about width
does.

The lesson is narrower than either doc: **a sweep that never crosses the
boundary cannot locate it.** The intensity sweep was told exactly this at
the time — it recorded "nothing here is a loss, so nothing here locates a
refusal boundary" — and the fix was to sweep the other axis.

## Why the width dependence exists

From the intensity sweep: GPU cost is dominated by bytes moved and is
almost independent of arithmetic (16x the ops moved GPU time 4-15%),
while CPU cost scales with ops x elements. Adding a fixed per-dispatch
overhead:

    gpu(n) ~= overhead + k * width * n
    cpu(n) ~=            c * n

Break-even is `n* = overhead / (c - k*width)`. The denominator shrinks as
width grows, so `n*` rises faster than linearly and diverges as
`k*width` approaches `c` — which is exactly what `long[]` (>1M) does
against `byte[]` (~32K).

The floor is visible directly in the data: GPU time bottoms out around
**62-66 us per call** for every type at n <= 4096, independent of size
and width. That is the fixed dispatch overhead, and it is what a
too-small admission is buying.

## Recommendation

1. **Raise the default.** At minimum to ~32,768, which is break-even for
   the *friendliest* width at the *worst* intensity. Anything below that
   loses for every type measured here.
2. **Scale it by element width** — or equivalently, threshold on BYTES
   rather than elements. A bytes-based threshold of roughly 256 KB-1 MB
   would approximate the `byte[]`/`int[]` break-evens; the 8-byte types
   need more still.
3. **Better: make it intensity-aware.** These numbers are all ops=1. The
   intensity sweep shows 5.6-22x wins at ops=16 for the same n=2^20, so a
   threshold set at the ops=1 break-even would refuse profitable
   high-intensity work. The well-founded predicate weighs estimated
   ops-per-element against bytes moved. This document supplies the
   constants that design needed and did not have.

Nothing here changes a default. That is deliberate: the fix is a
behaviour change on the GPU admission path, and it wants its own change
with a kill switch and a regression run, not a number edited into a
`default_value_t` at the end of a measurement.

## Limits

* One device, one shape (elementwise map, output size = input size), cold
  mode only. A kernel whose output is much smaller than its input (a
  reduction) moves fewer bytes back and should break even sooner.
* ops=1 throughout. The break-even falls as intensity rises; see the
  sibling document for that axis.
* `LOSS_AT=0.90` in the harness marks a clear loss rather than testing
  `< 1.00`, because the largest sizes sit at parity inside the ~10-17%
  round-to-round noise on this box — `long[]` at n=2^20 read 1.04x,
  0.98x and 0.85x on three separate runs. A bare `< 1.00` test would name
  whichever landed low as the crossover.
* Under `--gpu` the JIT gate denies the `run` dispatcher
  (`calls-eligible-kernel`), a constant adder against the GPU in every
  cell. It cannot have manufactured the large wins at n=2^20, and it does
  inflate the losses at small n — where the 62-66 us dispatch floor
  dominates anyway.
