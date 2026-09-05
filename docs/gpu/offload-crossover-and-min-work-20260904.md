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

## Correction, 2026-09-05: recommendation 1 below is WRONG

The recommendation to "raise the default to ~32,768" was drawn from the
ops=1 curve alone. Sweeping `n` at ops=4 and ops=16 as well
(`bench-gpu/crossover-n.sh`, `OPS=4` / `OPS=16`) gives the joint surface,
and it says a threshold there would create new losses:

Lowest `n` at which the GPU still WINS:

| type | ops=1 | ops=4 | ops=16 |
| --- | --- | --- | --- |
| `byte[]`   | ~32,000     | 16384 (1.10x) | 8192 (1.74x) |
| `int[]`    | ~65-130,000 | 65536 (2.00x) | 8192 (1.45x) |
| `long[]`   | > 1,048,576 | 65536 (1.11x) | 8192 (1.25x) |
| `double[]` | ~250K-1M    | 65536 (1.72x) | **2048 (1.21x)** |

A 32,768 threshold would refuse `double[]` at n=2048 (1.21x), `double[]`
at n=4096 (2.25x), `byte[]` at 8192 (1.74x), `int[]` at 8192 (1.45x) and
`long[]` at 8192 (1.25x). It would fix a 5x loss by manufacturing a
2.25x one.

**No scalar element threshold is correct.** Break-even moves by roughly
two orders of magnitude along the intensity axis alone -- `double[]` goes
from ~1M elements at ops=1 to 2048 at ops=16 -- so any single number is
badly wrong somewhere in the space. The current 4096 is not simply "too
low": it is far too low at ops=1 and about right at ops=16, which is
why it survives on compute-heavy kernels and loses 5x on transfer-bound
ones.

That is the second time a single-axis reading here proved too strong.
The sibling document concluded "do not scale by width" from one size;
this one concluded "raise to 32,768" from one intensity. **A threshold
over a two-dimensional space cannot be sited from a one-dimensional
slice**, in either direction.

## Recommendation (item 1 superseded -- see above)

1. ~~**Raise the default.** At minimum to ~32,768, which is break-even for
   the *friendliest* width at the *worst* intensity.~~ **Retracted**: the
   joint surface shows this refuses profitable high-intensity work.
2. **Scale it by element width** — or equivalently, threshold on BYTES
   rather than elements. A bytes-based threshold of roughly 256 KB-1 MB
   would approximate the `byte[]`/`int[]` break-evens; the 8-byte types
   need more still.
3. **The only correct fix is an intensity-aware predicate.** Not "better"
   -- the only one, now that the surface shows no scalar threshold works.
   The analyzer already walks the kernel body, so an ops-per-element
   estimate is available where the admission decision is made; weigh it
   against bytes moved rather than counting elements. The surface above
   is the calibration data that design needed and did not have.

   ~~A cheap interim: apply the threshold to `elements * width / ops`, a
   constant near 64 KB/op.~~ **RETRACTED 2026-09-05 -- that rule was
   never checked against this grid and is badly wrong.** Evaluated at the
   break-even cells it spans 512 bytes to 8,388,608, a **16,384x
   spread**; it is the worst of the three obvious products (`n*ops` and
   `n*ops/width` are both ~32x). Proposing it in the same commit that
   criticised single-axis reasoning was the same error one level up:
   an untested rule of thumb.

   No product of `n`, `ops` and `width` can be constant, because the
   model is a RATIO. See below.

## The cost model, fitted and validated

Break-even is `n* = overhead / (c*ops - k*w)`, so the predicate has to be
the cost comparison itself, not a product:

    admit  iff   n * (L + c*ops)   >   overhead + k * bytes_moved
                 \____ CPU ____/       \______ GPU _______/

`bytes_moved` is `2*w*n` (in and out). Fitted to the sweeps on this box:

| term | value | how |
| --- | --- | --- |
| `overhead` | **64,152 ns** | intercept of gpu_ns vs n, per type: 63716 / 63815 / 64173 / 64904 |
| `k` | **0.136 ns/byte** | slope / (2*w): 0.114 / 0.127 / 0.123 / 0.180 |
| `L` | ~1.0 ns | loop cost per element, from cpu_ns/n at ops=1 vs 4 |
| `c` | ~0.9 ns | arithmetic cost per op per element, same fit |

The GPU half is strikingly consistent -- four types agree on the
overhead within 2% and on the transfer slope within a factor of 1.6 --
which is what makes the transfer-bound reading more than a story.

**Validated against all 80 measured cells** (4 types x 3 intensities x
6-8 sizes):

| predicate | correct | admits a LOSS | refuses a win |
| --- | ---: | ---: | ---: |
| fitted cost model | **77/80 (96%)** | **0** | 3 |
| current `n >= 4096` | 47/80 (59%) | — | — |

It never admits a loss -- every error is a conservative refusal -- and
all three misses are `double[]`, which the CPU half underfits at high
intensity (predicted 21.5 ns/element at ops=16, measured 40.8). A
per-type `c`, or simply a safety factor on the CPU side, would recover
them.

That is the shape to implement, and the constants are box-specific:
they want re-fitting on any device this ships to, which is an argument
for measuring them at startup rather than baking them in.

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
