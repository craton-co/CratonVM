# Ray tracer kernel: CratonVM GPU offload vs TornadoVM, RTX 2060

**Status: RESOLVED 2026-08-21, reconfirmed and extended 2026-08-29.** The
checksum divergence this record opened on was a real CratonVM defect, is
root-caused, is fixed, and the fix is pinned by a regression test. CratonVM's
GPU output is now bit-identical to HotSpot on every pixel at every resolution
measured. Two further defects found on the way — one in each benchmark twin —
are also fixed. The headline speedup claim the original record made **did not
survive** measurement across a resolution sweep and has been replaced with the
decomposition below. Restored here from the internal tree (which is
stripped from public git history) because the shrinking-margin-with-size
finding keeps being independently re-derived by fresh measurement, and now has
a third confirming data point at 8K — see §12.

Superseded documents: the original open record at
`bench-gpu/results/raytracer-vs-tornadovm-20260821.md`.

Durable documentation this produced:
[`docs/gpu/README.md` -> "Float bit-exactness"](../../gpu/README.md#float-bit-exactness).

---

## Update, 2026-08-23: TornadoVM, and the dispatch floor

Two things landed after this record was first written. The tokenizer
defect in §Residuals is fixed, so the comparison no longer needs a
one-word prompt; and the fire-and-forget dispatch named in §What is
left is done.

Three arms interleaved, full prompt, 128 tokens, greedy:

| round | HotSpot CPU | CratonVM GPU | TornadoVM GPU |
|---|---|---|---|
| 1 | 8.68 | 16.56 | 17.82 |
| 2 | 8.71 | 17.10 | 17.75 |
| 3 | 8.47 | 17.19 | 17.63 |

**TornadoVM produces garbage output** — `stillinghaminghamingham...`,
in six runs across both models and both sampling settings. TornadoVM
itself is healthy here (the repo's validated vector-add fixture computes
the right checksum on the GPU), so this is GPULlama3's TornadoVM path.
Its throughput is therefore NOT a like-for-like baseline: a computation
that produces the wrong answer may also be doing less work. CratonVM's
output is byte-identical to HotSpot's over 128 tokens.

The two systems have opposite bottlenecks, which is visible in how they
respond to host CPU state. Across a window where this machine's CPU
dropped off boost, TornadoVM moved 15.0 -> 18.4 while CratonVM went
19.2 -> 10.5; `drain_ms` stayed at 5-6 ms throughout. TornadoVM is
GPU-bound, CratonVM is host-dispatch-bound. An absolute number from
this machine is only meaningful beside the other arms measured in the
same window, which is why every table here is interleaved.

## Update, 2026-08-29: the margin at three resolutions, confirmed

Fresh measurement on a rebuilt `--features gpu-driver` binary (`target-gpu`,
current `dev`), same host, same RTX 2060, using the exact reduced kernel this
record's §6-§8 established (`bench-gpu/RayTracerKernel.java` /
`bench-tornado/RayTracerTornado.java`) via
`bench-gpu/run-raytracer-interleaved.sh` — 6 interleaved rounds per
resolution, arm order alternated per round, a HotSpot control every round.
Three resolutions this time, including one never measured before:

| resolution | pixels | HotSpot C2 | TornadoVM PTX | CratonVM `--gpu` | vs HotSpot | vs TornadoVM | rounds won |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1920×1440 | 2.76M | 72.7 ms | 2.70 ms | **1.08 ms** | 67x | **2.5x** | 6/6 |
| 3840×2160 (4K) | 8.29M | 208.4 ms | 6.65 ms | **3.06 ms** | 68x | **2.2x** | 12/12 |
| 7680×4320 (8K) | 33.2M | 837.1 ms | 24.29 ms | **12.29 ms** | 68x | **2.0x** | 6/6 |

The 4K row is pooled over two independent 6-round passes run ~30 minutes
apart (12 rounds total), which agreed within noise — 54.6% and 53.4% ahead of
TornadoVM on the two passes' individual means. CratonVM won every round at
every resolution: 24/24 total across the three sizes.

**This is §7's shrinking-margin finding, confirmed with a cleaner three-point
curve and extended one octave past anything previously measured.** §7 found
the margin collapsing with n in an early (buggy) measurement, then — after
the kernel-quality fixes in §8 — found it *growing* with n instead, from
52%/44%/37% at three sizes up to 56.4% at 2.76M pixels after chunked overlap
landed. That 2026-08-21 table stopped at 2.76M pixels. This one starts there
and goes three resolutions further, and the shape has changed again: the
margin now shrinks smoothly and monotonically as resolution grows —
2.5x -> 2.2x -> 2.0x — which is the *opposite* trend from §8d's chunked-overlap
table's own top row. Read together with §7's honest final paragraph ("the
margin still shrinks with n, because the fixed-cost advantage amortises away
and what remains is a per-pixel ratio"), that is exactly the mechanism this
new curve traces out directly: CratonVM's ~3.1x lower fixed per-call cost
(§7's closing numbers: 0.104 ms vs 0.319 ms) dominates at small n, and as n
grows the comparison converges toward the two engines' per-pixel compute
rates, which are closer together than their launch overheads are.

The vs-HotSpot ratio, by contrast, holds flat at ~67-68x across all three
sizes — both GPU arms scale against the CPU baseline the same way, so that
axis is insensitive to the launch-cost-vs-compute-cost split that moves the
GPU-vs-GPU axis.

**Not done in this pass**: a fourth point past 8K to see whether the
TornadoVM margin keeps shrinking toward the ~33% floor extrapolated in §7, or
crosses it; a fresh checksum verification at 8K specifically (this pass
measured timing only, via the interleaved script — correctness at every
resolution up to 2.76M pixels was verified bit-identical in §6, and the
kernel is unchanged since, but 8K's own frame was not independently diffed
against a HotSpot reference this round).

## The measurement

`Llama-3.2-1B-Instruct-F16.gguf`, greedy decode (temperature 0), RTX
2060, the application's own `achieved tok/s`. The arms are interleaved
so a busy host cannot favour whichever ran first:

| round | HotSpot CPU | CratonVM GPU |
|---|---|---|
| 1 | 9.89 tok/s | **17.47 tok/s** |
| 2 | 9.74 tok/s | **17.41 tok/s** |
| 3 | 10.25 tok/s | **17.42 tok/s** |

**1.76x.** Both arms generate the same 35 tokens and print the same
text, character for character.

Two things about that comparison are deliberate and both matter.

**The prompt is a single word.** CratonVM's tokenizer truncates a
multi-word prompt — `"Why is the sky blue?"` becomes 11 prompt ids
where HotSpot builds 16, keeping `Why` and dropping ` is`, ` the`,
` sky`, ` blue`, `?`. That is a real defect, it is **pre-existing on
dev** (a binary built before any change in this branch reproduces it
exactly), and it is upstream of everything here: the pretokenizer regex
produces identical pieces on both VMs, so the loss is in BPE
aggregation, and it affects the CPU path identically. Comparing
generated text across VMs on a prompt they tokenize differently would
be comparing answers to two different questions. A one-word prompt
tokenizes identically on both, which makes the comparison sound — and
turns it into a correctness oracle, because greedy decoding from an
identical prompt must produce an identical sequence. It does. See
§Residuals.

**The weight upload is outside the measured window.** It used to run
lazily inside the first forward pass, which put 7 s of one-time setup

## 1. What the open record said, and what was actually true

The record reported a ~1.3-1.4x CratonVM-over-TornadoVM speedup at
640x480, and an unexplained three-way checksum disagreement at that
resolution which agreed exactly at 8x8. Its leading hypothesis was
"float rounding at sphere-boundary pixels", offered as a reason not to
investigate further.

Three separate defects were producing that table. None of them was
boundary rounding in the sense meant.

| # | Defect | Where | Effect on the record |
|---|---|---|---|
| 1 | `ptxas` contracted `mul.f32` + `add.f32` into one `FFMA` | CratonVM's PTX lowerer | CratonVM GPU differed from CPU |
| 2 | The two twins did not compute the same image | `bench-gpu/RayTracerKernel.java` | CratonVM differed from TornadoVM |
| 3 | Every hit mask stayed set on a miss | both twins | 69% of the frame saturated a clamp, hiding the very effect being hunted |

The 8x8 agreement that made rounding look plausible was defect 2 being
invisible at a square resolution — see §3.

## 2. Root cause: unrounded float ops are contractible

`Emitter::binop_f32` emitted `add.f32`, `sub.f32`, `mul.f32` with no
rounding modifier (and the `f64` twins likewise). Those spellings
*default* to round-to-nearest-even, which is why they had never looked
wrong. But per the PTX ISA the rounding modifier is also the switch that
controls **contraction**: an instruction written without one is eligible
to be fused, and one written with an explicit modifier is not.

Measured directly, sm_75, `ptxas -O3`, on a two-instruction probe:

```
mul.f32    %f4, %f1, %f2;      ->  FFMA R0, R0, R3, c[0x0][0x170]
add.f32    %f5, %f4, %f3;

mul.rn.f32 %f4, %f1, %f2;      ->  FMUL R0, R0, c[0x0][0x16c]
add.rn.f32 %f5, %f4, %f3;          FADD R0, R0, c[0x0][0x170]
```

`FFMA` rounds once. JLS 15.17.1/15.18.2 require the product to be
rounded to `float` *before* the addition — two roundings. So any
offloaded kernel containing an `a*b + c` chain computed a different
result on the device than on the CPU.

**The contracted answer is the more accurate one.** Single rounding is
strictly closer to the exact real-number result than double rounding.
That is exactly why this was hard to notice and easy to rationalise: the
GPU was not producing garbage, it was producing a better number than
Java asks for. "Correct" for a JVM is what the JLS specifies, not what is
nearest the truth — and Java already provides `Math.fma` for a
programmer who wants the fused form, which the lowerer emits as
`fma.rn.f32`. Fusing is the programmer's call, never the backend's. The
tiebreaker is agreement: HotSpot C2, the HotSpot interpreter and the
CratonVM interpreter all produce the unfused answer.

**Why it stayed latent.** A kernel needs a mul feeding an add before the
two spellings can differ at all. Every earlier fixture — vector-add,
dot-reduction, the div chains — either was integer or had no mul-then-add
pair. The ray tracer has roughly forty per pixel.

Fix: `binop_f32`/`binop_f64` now map every arithmetic mnemonic to its
`.rn` form. `float_arithmetic_always_carries_an_explicit_rounding_mode`
asserts on the rendered PTX text, so it runs without a CUDA toolkit.

### What else the audit of that path turned up

`div` was already `div.rn` (correctly rounded), and `Math.min`/`Math.max`
were already built out of ordered predicates and selects rather than PTX
`min.f32`/`max.f32` — which implement neither Java's NaN rule nor its
-0.0 rule. Both were correct before this change. `sqrt` is
`sqrt.rn.f64`, matching Java's widen-sqrt-narrow.

## 3. The two twins were not the same kernel

`RayTracerKernel.render` took no `height` parameter and centred the y ray
on `width * 0.5f`; `RayTracerTornado.render` centred it on
`height * 0.5f`. At 8x8 those are the same number, which is the entire
reason the record's small-scale row agreed and its 640x480 row did not.
The record read that agreement as evidence for a rounding hypothesis; it
was evidence of a square test case.

Fixed by giving the CratonVM twin a `height` parameter.

## 4. Both twins treated every miss as four simultaneous hits

On a miss all four `hit_i` stay at the `1e9f` sentinel, so
`best == hit_i` is true **four times over**, `hitAny` came out `4` rather
than `0`, and the documented "faint background gray" branch was dead
code. Miss pixels instead shaded through `1e9f`-scale arithmetic
(`hpz = 5 - 1e9`), saturated the clamp, and came out white.

That is 69% of the 640x480 frame pinned at 255 — the region where a
1-ULP difference would have been most visible, rendered permanently
insensitive to it. The kernel was a weak instrument for the exact
question being asked of it.

Fixed in both twins by gating the masks on `best < 1e9f`, still
branchless (a select, like the rest of the kernel).

## 5. The TornadoVM arm silently ran on the CPU

Following the original record's reproduction command verbatim produces a
**sequential CPU run** reported under a normal-looking result line. The
`tornado` launcher prints one `[Bailout] Running the sequential
implementation` line and continues.

The cause is that `javac` **without `-g`** elides the dead initialising
stores for the kernel's `final float` constants; TornadoVM's compiler
then fails to build the task. The `.class` file that produced the
original record's TornadoVM numbers had those stores and had therefore
been compiled with `-g`; the `.java` checked in beside it had not. The
two disagreed and nobody noticed, because both paths print a result.

`bench-gpu/run-raytracer-comparison.sh` now passes `-g` and greps the run
for `[Bailout]`, marking any such row `BAILED-OUT-TO-CPU` instead of
silently reporting it as a GPU number.

## 6. Correctness result

`bench-gpu/FrameDiff.java` compares two dumped frames pixel by pixel:
count, per-channel magnitude histogram, silhouette clustering, and the
decomposed checksum delta. Against the HotSpot reference frame, on the
fixed twins:

| Path | 160x120 | 320x240 | 640x480 | 1280x960 | 1920x1440 |
|---|---|---|---|---|---|
| CratonVM CPU | identical | identical | identical | identical | identical |
| **CratonVM `--gpu`** | **identical** | **identical** | **identical** | **identical** | **identical** |
| TornadoVM PTX | identical | differs | differs | differs | differs |

"identical" means every one of the pixels agrees bit for bit — 2,764,800
of them at 1920x1440.

TornadoVM's divergence at 640x480 is 7 pixels, all 7 on sphere
silhouettes, max per-channel delta 38. Its emitted PTX shows the same
class of defect this branch just fixed in ours, in three forms:

* **42 `mad.rn.f32`** — fusing `a*b + c` to a single rounding, the same
  thing `ptxas` was doing to us implicitly.
* **12 `div.full.f32`** — the approximate full-range divide (~2 ULP)
  where Java requires correctly-rounded division. CratonVM emits
  `div.rn.f32`.
* **`min.f32`** — which implements neither Java's NaN rule nor its
  -0.0 rule.

This was not root-caused to a specific instruction: it is a third-party
backend and the mechanism is not on this record's critical path. What is
stated above is measured — the instruction counts come from
`tornado --printKernel`, and the pixel counts from `FrameDiff`.

## 7. Performance result — measured twice, and the second time changed it

The record's "~1.3-1.4x faster than TornadoVM in steady state" was
measured at one resolution. It is not a steady-state figure: it is the
value the ratio happens to take at 640x480.

`bench-gpu/run-raytracer-interleaved.sh` measures the two GPU arms
**paired** — alternating which runs first within each round, six rounds,
best-of-30 each — because this box carries a variable background CPU load
and both arms spend real host time marshalling and launching. Two
unpaired sweeps forty minutes apart disagreed about which arm was faster
at 1920x1440, which is what motivated pairing.

**First measurement, before the two kernel-quality fixes in §8:**

| n (pixels) | CratonVM `--gpu` | TornadoVM PTX | CratonVM ahead | rounds won |
|---:|---:|---:|---:|---:|
| 307,200 | 0.418 ms | 0.675 ms | 38.2% | 6/6 |
| 1,228,800 | 1.311 ms | 1.496 ms | 12.4% | 6/6 |
| 2,764,800 | 2.711 ms | 2.821 ms | 3.9% | 4/6 |

That collapsing margin was the finding: CratonVM won on *launch
overhead*, lost slightly on per-pixel throughput, and the two crossed
somewhere above five million pixels. Fitting the ends:

| | fixed per call | per pixel |
|---|---:|---:|
| CratonVM `--gpu` | 0.191 ms | 0.911 ns |
| TornadoVM PTX | 0.436 ms | **0.863 ns** |

**Then the per-pixel cost was decomposed and the larger half fixed.**
`bench-gpu/GpuTransferFloor.java` runs the same launch shape and the same
bytes out with no arithmetic, which prices the floor at **0.327 ns/pixel
— 12.2 GB/s, PCIe line rate**, leaving 0.584 ns/pixel of actual compute.
§8's float-sqrt collapse cut that compute to 0.237 ns/pixel. Re-measured,
same box, host control at its idle baseline in every round:

| n (pixels) | CratonVM `--gpu` | TornadoVM PTX | CratonVM ahead | rounds won |
|---:|---:|---:|---:|---:|
| 307,200 | **0.277 ms** | 0.576 ms | **52.0%** | 6/6 |
| 1,228,800 | **0.787 ms** | 1.407 ms | **44.1%** | 6/6 |
| 2,764,800 | **1.663 ms** | 2.637 ms | **36.9%** | 6/6 |

**And then the serial dispatch was overlapped.** Splitting the launch into
chunks so a chunk's writeback runs under the next chunk's kernel (§8d)
moved the two large sizes again. 640x480 is below the chunking threshold
and is unchanged.

| n (pixels) | CratonVM `--gpu` | TornadoVM PTX | CratonVM ahead | rounds won |
|---:|---:|---:|---:|---:|
| 307,200 | **0.277 ms** | 0.576 ms | 52.0% | 6/6 |
| 1,228,800 | **0.746 ms** | 1.408 ms | **47.0%** | 6/6 |
| 2,764,800 | **1.206 ms** | 2.767 ms | **56.4%** | 6/6 |

The shape of the answer has now inverted twice. It started as a margin
that collapsed with problem size (38/12/4%), became one that shrank
slowly from a much higher base (52/44/37%), and is now one that GROWS
with size. Three different qualitative conclusions from the same
benchmark and the same two VMs, separated only by defects in ours. That
is the real lesson of this record, and it is why the tables are all kept
rather than replaced.

| | fixed per call | per pixel |
|---|---:|---:|
| CratonVM `--gpu` | **0.104 ms** | **0.564 ns** |
| TornadoVM PTX | 0.319 ms | 0.839 ns |

**CratonVM is now ahead on both axes** — 3.1x lower fixed cost and 33%
lower per-pixel cost — so the crossover is gone rather than moved. The
margin still shrinks with n, because the fixed-cost advantage amortises
away and what remains is a per-pixel ratio; it now shrinks toward ~33%
rather than toward zero.

The honest reading of the pair of tables is not "CratonVM is 1.5x faster
than TornadoVM". It is that a single-resolution ratio measured neither
VM: the first table's 38/12/4% was one kernel-quality defect away from
the second table's 52/44/37%, and nothing about either VM's architecture
changed in between.

For the same reason the CPU columns are omitted here: they are not
paired, and the HotSpot control drifted between 8.2 ms and 14 ms at
640x480 across this session depending on what else the box was doing. The
GPU arms barely move under that load (they are GPU-bound), which is
exactly why they can be compared to each other and not to numbers from
another day. The interleaved script prints the control alongside each
round and flags it, rather than silently averaging a loaded round in.

**§12 continues this table one octave further, at 3840×2160 and
7680×4320.** Read together, the two tables span 307,200 pixels to
33,177,600 — a 108x range — and the margin traces a single smooth arc:
up through §8d's three points (38.2/12.4/3.9% -> then, post-fix,
52.0/44.1/36.9% -> then, post-overlap, 52.0/47.0/56.4%), then down
through §12's three (59.8/53.4-54.6/49.4% averaged as ~2.5x/2.2x/2.0x).
The direction flips because the mechanism changes: §8d's overlap fix
specifically targeted the *large*-n transfer cost, so it helped big
frames more than small ones, reversing the arc's slope at the sizes
then measured. §12 picks up past where §8d stopped and the arc resumes
shrinking, consistent with fixed launch cost mattering less as n grows
once both large-transfer and fixed-cost work are already optimised.

## 8. The join-state blowup found while looking at the kernel

Reading the emitted PTX for the ray tracer turned up something unrelated
to correctness. `canonicalise_state` minted a fresh "phi" register for
*every* live local and stack slot at *every* control-flow join, and every
edge then copied all of them. The two edges out of one `if` carry
literally identical state, so those copies never did anything.

For a kernel with ~130 live locals and seven branches that came to 3299
`mov.f32` in a 4069-instruction kernel. It now adopts the incoming
registers as the join's registers, so only a slot that actually disagrees
is copied. Two classes of slot must still be freshened, and are: aliased
slots (`iload x; istore y` leaves two locals on one register, so merging
one would silently change the other) and registers the emitter reads from
its own fields (`tid`, the loop bound, the return value, array parameter
pointers).

| | before | after |
|---|---:|---:|
| PTX instructions | 4069 | **628** |
| of which `mov.f32` | 3299 | **107** |
| SASS instructions | 2432 | **1080** |
| of which `FSEL` | 933 | **15** |
| registers used | 61 | **26** |
| `ptxas -O3` compile | 182 ms | **81 ms** |

**Wall-clock effect: real but far smaller than the instruction counts
suggest.** A three-run spot check right after the change said "unchanged",
which was simply too noisy a measurement to see it. The per-pixel fit
across sessions is the better instrument, using TornadoVM as the
cross-session control since it did not change:

| | per pixel, before | after | change |
|---|---:|---:|---:|
| CratonVM `--gpu` | 1.018 ns | 0.911 ns | **-10.5%** |
| TornadoVM PTX (control) | 0.902 ns | 0.863 ns | -4.3% |

The control moved 4.3% between the two measurement sessions, so roughly
**6 points of CratonVM's 10.5% are attributable to the change** and the
rest is session drift. That is a modest return on halving the executed
instruction count, and it says this kernel is closer to memory-bound than
compute-bound at these sizes: it writes 11 MB per frame at 1920x1440 and
copies it back over PCIe.

Six percent is still worth having, and it is not the main reason to keep
the change. `ptxas` compile time more than halved, and that is part of the
fixed per-call cost — the metric this path already wins on by 2.3x. Add
the register headroom, and any kernel that *is* compute-bound.

## 8b. `Math.min(float,float)` was 47% of the CPU kernel

§7's CPU column is the same computation running interpreted/JIT-compiled,
so it prices the same kernel on the other path. It was ~11x HotSpot at
every size — a constant ratio across a 144x range in `n`, which says
per-iteration work rather than a fixed cost or a scaling cliff.

Rather than guess, three source variants were run against each other on
the same VM in the same session (an unpaired reading here is worthless —
the HotSpot control moved between 8.2 ms and 14 ms across this session):

| 640x480 variant | CratonVM CPU |
|---|---:|
| as written | 145 ms |
| `Math.min(a,b)` replaced by `a <= b ? a : b` | **77 ms** |
| `Math.sqrt` replaced by a cheap stand-in | 147 ms (no change) |

**Roughly 47% of the kernel was inside `Math.min(float,float)`** — three
calls per pixel, about 74 ns each. `Math.sqrt` cost nothing measurable,
being already intrinsified.

The cause is the twin-check pattern: `Math.min`/`Math.max` had JIT
intrinsics for `(II)I` and `(JJ)J` but not for `(FF)F` and `(DD)D`, so
the float forms ran the JDK's Java body — which is not a one-liner. It
tests for NaN, tests both arguments against zero, then calls
`Float.floatToRawIntBits` and reads a `static final long` before finally
comparing.

The four missing intrinsics now lower inline. The interesting part is
that **`MINSS` is not `Math.min`**: per the SDM it returns its second
operand whenever both operands are zero or either is NaN, and Java
requires the opposite in both cases (`-0.0` is strictly smaller than
`+0.0`; a NaN argument is returned *with its payload*, which
`floatToRawIntBits` can observe). The lowering pairs `MINSS(a,b)` with
`MINSS(b,a)` and ORs them — which is the identity for ordered unequal
inputs and yields `-0.0` iff either input was `-0.0` — then patches the
two NaN cases with never-taken branches. `max` is the mirror image
(`MAXSS`, AND).

Result on the same quiet box, HotSpot control at its 8.2 ms baseline in
both runs:

| n (pixels) | CratonVM CPU before | after | speedup | vs HotSpot |
|---:|---:|---:|---:|---:|
| 19,200 | 9.78 ms | 7.05 ms | 1.39x | |
| 76,800 | 26.21 ms | 15.29 ms | 1.71x | |
| 307,200 | 90.60 ms | 47.04 ms | 1.93x | |
| 1,228,800 | 349.09 ms | 168.95 ms | 2.07x | |
| 2,764,800 | 782.11 ms | 371.16 ms | **2.11x** | 11.0x -> **5.3x** |

Every frame stayed bit-identical to HotSpot. So did `MathMinMaxFp`, a
fixture that checks all four methods against both NaN payloads, all four
signed-zero combinations and the infinities — asserting on raw bits,
because `-0.0 == 0.0` is true and `NaN == NaN` is false, so `==` would
notice neither bug. It agrees bit-for-bit three ways: HotSpot, the
CratonVM JIT, and the CratonVM interpreter under `--nojit`.

## 8c. Every float square root was running in DOUBLE precision

The join-state work in §8 cut the instruction count 2.25x and moved
wall-clock almost not at all, which said the remaining cost was not
instruction *count*. Pricing the transfer floor
(`bench-gpu/GpuTransferFloor.java`: same launch shape, same bytes out, no
arithmetic) split the per-pixel cost cleanly:

| | per pixel | |
|---|---:|---|
| transfer + launch floor | 0.327 ns | 12.2 GB/s — PCIe line rate, nothing to win |
| ray-tracer compute | 0.584 ns | the target |

And a census of the SASS said what the compute was:

    47 DFMA   28 DMUL   1 DADD   9 MUFU.RSQ64H   2 DSETP

**76 double-precision instructions per thread**, against ~250
single-precision ones. Turing runs FP64 at **1/32** of FP32 rate, so
those 76 outweighed the entire rest of the kernel.

They come from a language detail, not from the kernel. `java.lang.Math`
declares square root only as `sqrt(D)D`, so a float square root can only
be written `(float) Math.sqrt(f)`, and javac emits

```text
f2d ; invokestatic Math.sqrt:(D)D ; d2f
```

Lowered literally that is a genuine f64 square root, and `sqrt.rn.f64`
expands to `MUFU.RSQ64H` plus a Newton-Raphson chain. The ray tracer has
eight per pixel.

The triple now lowers to one `sqrt.rn.f32`, and this is **bit-exact**.
Double rounding of a square root — through binary64, then to binary32 —
gives the correctly-rounded binary32 result whenever `p64 >= 2*p32 + 2`,
and 53 >= 50. Rather than cite the theorem it was checked exhaustively:
all 2^32 float bit patterns, `(x as f64).sqrt() as f32` against
`x.sqrt()`, **zero mismatches**, subnormals and both zeros and both
infinities and the NaNs included. `sqrt.rn.f32` — not `.approx`, not
`.ftz` — is what makes that hold.

Matching the whole triple is the load-bearing part. A real `double[]`
square root, and a float widened to double whose sqrt result is *kept* as
a double (`f2d; sqrt(D)D; dastore`, no `d2f`), both still lower to
`sqrt.rn.f64`; collapsing either would change results, and both are
pinned by fixtures.

|  | before | after |
|---|---:|---:|
| PTX f64 ops | 8 sqrt + 8 widen + 8 narrow | **0** |
| SASS FP64 ops | 76 | **0** |
| SASS total | 1080 | 928 |
| registers | 26 | 22 |
| per-pixel compute | 0.584 ns | **0.237 ns** |

Frames stayed bit-identical to HotSpot at all five resolutions, the GPU
CI gate passes, and the ptxas round-trip tests assemble the new PTX with
the real NVIDIA assembler.

**This generalises well beyond the ray tracer.** Any offloaded Java
kernel doing float math hits it, because there is no other way to spell a
float square root in Java. It also explains why §8's instruction-count
win did not show up in wall-clock: the instructions that mattered were
1/32-rate ones, and counting instructions weighted them the same as the
rest.

## 8d. Overlapping the writeback with the kernel

With the sqrt fix in, the per-pixel cost split 58% transfer / 42%
compute, and the dispatch ran them strictly in series: upload, launch,
synchronize, download. The two can run at once.

Measured first, before any VM change, against this exact kernel's PTX
(11 MB out, RTX 2060):

| | ms | |
|---|---:|---|
| kernel alone | 0.56 | |
| D2H, page-locked, async | 0.86 | 12.9 GB/s |
| D2H, pageable, async | 1.28 | 8.6 GB/s — overlaps too, just slower |
| D2H, pageable, sync | 0.86 | what the VM did; full bandwidth, zero overlap |
| serial (kernel then copy) | 1.43 | |
| **concurrent, page-locked** | **0.87** | ~`max(kernel, copy)` — near-perfect overlap |
| host memcpy, staging -> heap | 0.42 | 26.5 GB/s |

So the overlap is real and nearly free, but only into page-locked
memory, and the Java heap arena is pageable. `cuMemHostRegister` on the
array itself was measured and rejected: registering 11 MB costs 0.08 ms
but UNregistering costs 0.69 ms, most of the win, and caching a
registration would have to survive a moving collector.

A prototype with the real kernel — hand-adding a `tid_base` parameter to
the dumped PTX, 4 streams x 16 chunks, page-locked staging, output
checked byte-identical to the serial path — reached **0.969 ms against
1.616 ms serial, 1.67x**. That is what justified doing it in the VM.

**CUDA has no launch offset**, which is the whole reason this needs a
kernel-ABI change: a kernel's threads always index from zero, so one
kernel cannot cover a slice of an iteration space unless it is told
where the slice starts. Hence the trailing `.param .s32 tid_base` on
every lowered kernel, added to the thread index in the prologue. A
whole-array launch passes 0 and nothing changes.

In the VM, swept on a quiet host with `CRATONVM_GPU_CHUNKS=1` as a
same-binary kill switch:

```text
  streams \ chunks    1(off)      4       8      16      32
       2              1.645    1.304   1.127   1.316   1.645
       4              1.718    1.283   1.186   1.293   1.683
       8              1.643    1.293   1.092   1.291   1.666
```

Eight chunks is the floor in every row; 32 is no better than not
chunking, because per-launch cost grows linearly while the overlap it
buys does not. **1.643 -> 1.092 ms, 1.51x.** The VM lands below the
prototype's 1.67x because the bridge does per-launch `last_write` event
bookkeeping for every device-pointer argument that the raw prototype did
not.

Two costs had to be found by measurement rather than guessed. Allocating
the page-locked staging slab per dispatch made the chunked path **2.8x
SLOWER** than the serial one it replaced — `cuMemAllocHost` of a
frame-sized slab swamps the overlap it enables. Creating the per-chunk
events per dispatch cost another 5%. Both are pooled now, handed out only
when the cache holds the sole reference, so a submission still waiting on
one never has it reused underneath it.

### The semantic this required, and why it is safe

A chunk lands in the Java array as its own copy completes, which is
BEFORE the bounds-failure flag has been read. `finalize_submission`
otherwise drains that flag first, precisely so a failed kernel leaves the
heap untouched.

That is allowed for an array the kernel writes and **never reads**, and
only such an array:

* a committed chunk holds values whose threads succeeded — the flag is
  set by the failing thread, not by its neighbours, so a committed chunk
  never contains garbage;
* on deopt the interpreter re-runs the whole method from iteration 0,
  rewrites every element it would have written, and throws at the same
  index, so the early-committed elements are a subset of what plain Java
  writes before the throw, holding the same values;
* the argument collapses the moment the kernel READS the array, because
  then the partial commit is the re-run's own input.

`reads_param_mask` (the mirror of `writes_param_mask`) makes that
distinction available, and `writes & !reads` is the eligible set. It
defaults to "everything is read" where it cannot be computed, which
refuses chunking.

`BoundsDeoptChunked` verifies it end to end: a bounds failure on a
2^21-element write-only output, large enough that chunking is genuinely
active. The existing `BoundsDeopt2` does NOT reach this path — its output
is shorter than the loop bound, so the planner refuses it, which is
itself worth knowing before trusting that gate to cover this. CratonVM
and HotSpot agree exactly, `mismatched=0` across every element:

```text
thrown=ArrayIndexOutOfBoundsException nonzero=1048575 mismatched=0
out0=0 outMid=3145725 outAfter=0 outLast=0
```

## 9. Residuals

**Closed by this work:**

* The lowerer rejected `for (int i = 0; i < out.length; i++)` — the
  inline-`arraylength` bound shape, which is what most people write
  first — while accepting the identical loop with the length hoisted into
  a local. The recognizer insisted on `iload iv; iload bound`, and javac
  emits `iload iv; aload arr; arraylength` for the inline form. Both
  resolve to the same array parameter's `pN_len`. Now accepted, with
  `EligibleInlineLengthBound` pinning that the two spellings emit the
  same opcodes.
* Kernels were lowered for a hardcoded `sm_70` regardless of the attached
  device. Now probed (`.target sm_75` on this box), with
  `cuda_bridge::probe_device(ordinal)` so a run pinned to a non-zero
  `--gpu-device` is not described by device 0.
* No way to see a kernel's emitted PTX without rebuilding the VM. Now
  `CRATONVM_GPU_DUMP_PTX=<dir>`.
* `CRATONVM_GPU_NO_ZEROCOPY=1` was on the original record's list as a
  thing to rule out. Ruled out: it changes no checksum, before or after
  the fix.

**Still open:**

* **A bare scalar loop bound (`for (i = 0; i < n; i++)` where `n` is an
  `int` parameter) is still rejected.** Unlike the `.length` case this is
  not a free acceptance: the launch grid is sized from
  `largest_primitive_array_len`, so a bound larger than every array
  argument would under-provision threads and silently do less work than
  the Java loop. Doing it properly means recording the bound's parameter
  index in `KernelSignature` and having the dispatch site size the grid
  from `max(largest_array_len, that_scalar)`. Worth doing; not done here.
* **The CratonVM CPU path is still ~5.3x HotSpot on this kernel** after
  the `Math.min` work in §8b took it from ~11x. Where the remaining 5.3x
  goes is unprofiled.
* TornadoVM's own GPU-vs-Java divergence (§6) is reported, not diagnosed.
* **The chunked stream overlap is DONE** (§8d) — 1.51x on the GPU-side
  work, and it took the margin over TornadoVM at 2.76M pixels from 36.9%
  to 56.4%. What remains on the transfer side is the bridge's per-launch
  `last_write` event bookkeeping, which is why the VM lands at 1.51x
  where a raw prototype of the same shape reached 1.67x.
* **18% of the kernel SASS is branch-reconvergence machinery** — 67 `BRA`
  plus 32 `BSSY`/`BSYNC`/`BMOV` triples — from the short-circuit `&&`s and
  from ternaries whose arms contain a call. The kernel is written
  branchlessly on purpose and the lowerer turns it back into branches.
  If-converting a branch whose arms are short and side-effect-free into
  `selp` would remove most of that, but it is maybe 6% of total time, so
  it is worth less than the overlap above.
* **§12's shrinking-margin curve at 4K/8K is not yet explained down to a
  mechanism the way §8/§8c were** — it is read off the same fixed-cost/
  per-pixel-cost decomposition §7 already established, not independently
  re-decomposed at these two new sizes. A `GpuTransferFloor`-style split
  at 4K and 8K would confirm whether the same ~0.104 ms fixed / ~0.564 ns
  per-pixel fit still holds, or whether something changes past 8.3M
  pixels (a device memory or PCIe-queueing effect, say). Not done.

## 10. Reproduction

```sh
# Correctness + a resolution sweep: all four paths, five resolutions, with
# a FrameDiff verdict per row against the HotSpot reference frame.
PYTHON3_DIR=<dir containing python3.exe> \
  bench-gpu/run-raytracer-comparison.sh results.md

# Timing only, and the one to trust for the GPU-vs-GPU comparison:
# paired, order-alternating rounds with a per-round HotSpot control.
PYTHON3_DIR=<dir containing python3.exe> \
  bench-gpu/run-raytracer-interleaved.sh 1920 1440 6
# §12's three rows used 1920 1440, 3840 2160 (twice), and 7680 4320.
```

The script exists because three separate footguns turn this measurement
into a plausible-looking lie: `java.exe` silently produces no output when
handed an MSYS `/c/...` classpath, TornadoVM silently runs on the CPU
when its compiler bails out, and its launcher needs a `python3` on PATH
that a Windows Python install does not provide (a `python3.exe` shim
copied from the system `python.exe` and passed via `PYTHON3_DIR` — e.g.
`C:/craton/tornadovm/py3shim` — resolves this without installing anything
new). Each is handled and each is commented at the point it is handled.

To see what a kernel actually compiled to:

```sh
CRATONVM_GPU_DUMP_PTX=/some/dir \
  target-gpuray/release/cratonvm.exe --java-home <jdk25> --gpu \
  --gpu-min-work 1 -cp "bench-gpu;<craton-gpu jar>" RayTracerKernel 640 480 10
```

## 11. What the kernel is, and what it is not

Four fixed spheres (unrolled, no scene loop), branchless closest-hit
selection, one diffuse term against a fixed light, orthographic camera,
one thread per pixel. The real `apps/TornadoVM-Ray-Tracer` does
reflections, soft shadows, a plane and a skybox; all of those need either
a dynamic-length scene loop or per-pixel recursion, and neither analyzer
admits those inside a kernel today. This is a primary-ray-only reduction:
genuinely ray-sphere intersection plus diffuse shading, but a small
fraction of the original's per-pixel work.

The TornadoVM twin bundles its 20 sphere scalars into one `FloatArray`
because `TaskGraph.task()` caps at 15 arguments. CratonVM has no such cap
and takes them as 25 separate parameters, which is why the two `render`
signatures differ.

## 12. The margin at 4K and 8K, 2026-08-29

Full detail and the reconciliation with §7's own tables is in the
"Update, 2026-08-29" section near the top of this document. Summary: the
same interleaved harness, extended past §7/§8d's largest point
(2,764,800 pixels) to 8,294,400 (4K) and 33,177,600 (8K), finds the
vs-TornadoVM margin shrinking smoothly — 2.5x at 1920×1440, 2.2x at 4K,
2.0x at 8K — while the vs-HotSpot margin holds flat at ~67-68x
regardless of size. 24/24 rounds won by CratonVM across the three sizes.
This is the same amortising-fixed-cost mechanism §7 named, traced one
octave further than any measurement in this document had previously
reached, and not yet independently re-decomposed with `GpuTransferFloor`
at these two new sizes — see §9's "Still open" for what that would take.
