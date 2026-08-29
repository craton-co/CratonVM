# Ray tracer kernel: CratonVM GPU offload vs TornadoVM, RTX 2060

**Status: RESOLVED 2026-08-21. Residuals CLOSED 2026-08-29.** The checksum
divergence this record opened on was a real CratonVM defect, is root-caused,
is fixed, and the fix is pinned by a regression test. CratonVM's GPU output
is bit-identical to HotSpot on every pixel at every resolution measured, now
including 8K and 11520×6480. Two further defects found on the way — one in
each benchmark twin — are also fixed. The headline speedup claim the original
record made **did not survive** measurement across a resolution sweep and has
been replaced with the decomposition below.

Every item §9 left open has been taken to an answer — see §13. Two of those
answers are that the residual's own hypothesis was wrong, which is why they
are written up rather than quietly dropped.

Superseded documents: the original open record at
`bench-gpu/results/raytracer-vs-tornadovm-20260821.md`, and the copy that
lived at `docs/known-issues/gpu/raytracer-vs-tornadovm.md` between
2026-08-23 and 2026-08-29 while the residuals were open.

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

**Both were done later the same day — see §13.1 and §13.2.**

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
7680×4320; §13.1 adds a fourth point at 11520×6480.** Read together, the two tables span 307,200 pixels to
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

**Every "Still open" item below was taken to an answer on 2026-08-29;
§13 is that pass.** The list is kept as written so the answers can be read
against the questions, including the two answers that are "the hypothesis in
this bullet was wrong".

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

**Was still open on 2026-08-21, and every one is answered in §13:**

* **A bare scalar loop bound (`for (i = 0; i < n; i++)` where `n` is an
  `int` parameter) is still rejected.** **DONE — §13.3, and the paired
  grid change this bullet correctly insisted on came with it.**
  Unlike the `.length` case this is not a free acceptance: the launch grid is sized from
  `largest_primitive_array_len`, so a bound larger than every array
  argument would under-provision threads and silently do less work than
  the Java loop. Doing it properly means recording the bound's parameter
  index in `KernelSignature` and having the dispatch site size the grid
  from `max(largest_array_len, that_scalar)`. Worth doing; not done here.
* **The CratonVM CPU path is still ~5.3x HotSpot on this kernel** after
  the `Math.min` work in §8b took it from ~11x. Where the remaining 5.3x
  goes is unprofiled. **PROFILED — §13.7.** About 3.3-3.5x of it is
  CratonVM's scalar float code; the rest is auto-vectorisation HotSpot
  does and CratonVM does not. There is no second `Math.min` in it.
* TornadoVM's own GPU-vs-Java divergence (§6) is reported, not diagnosed.
  **CLOSED as out of scope — §13.8.** §6 already names it at instruction
  level; the rest is inside a third-party backend.
* **The chunked stream overlap is DONE** (§8d) — 1.51x on the GPU-side
  work, and it took the margin over TornadoVM at 2.76M pixels from 36.9%
  to 56.4%. What remains on the transfer side is the bridge's per-launch
  `last_write` event bookkeeping, which is why the VM lands at 1.51x
  where a raw prototype of the same shape reached 1.67x. **That
  attribution was WRONG — §13.5.** The bookkeeping is gone now and the
  ratio did not move; a frame is 8 launches, so the whole saving here was
  bounded at two or three percent before anyone measured it.
* **18% of the kernel SASS is branch-reconvergence machinery** — 67 `BRA`
  plus 32 `BSSY`/`BSYNC`/`BMOV` triples — from the short-circuit `&&`s and
  from ternaries whose arms contain a call. The kernel is written
  branchlessly on purpose and the lowerer turns it back into branches.
  If-converting a branch whose arms are short and side-effect-free into
  `selp` would remove most of that, but it is maybe 6% of total time, so
  it is worth less than the overlap above. **BUILT, MEASURED, and it goes
  the WRONG WAY — §13.4.** It does remove the branches, and it costs 56%
  of the compute half, because "short" was the wrong screen: the arms are
  one instruction each and one of those instructions is a square root.
  Re-screened by weighted cost and swept, the best budget ties with the
  feature off. It ships opt-in.
* **§12's shrinking-margin curve at 4K/8K is not yet explained down to a
  mechanism the way §8/§8c were** — **DECOMPOSED in §13.6, and the
  fixed-cost model this bullet expected it to confirm turns out not to
  hold past 8K.** The bullet as written: it is read off the same
  fixed-cost / per-pixel-cost decomposition §7 already established, not independently
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

The residual pass in §13 added four more, all in `bench-gpu/`:

```sh
# Fixed-cost / per-pixel decomposition at any set of sizes (§13.6).
GPU_JAR=<craton-gpu jar>   bench-gpu/run-transfer-decomposition.sh <cratonvm.exe> 1920 1440 3840 2160                                           7680 4320 11520 6480

# Bit-exactness at one resolution, against a HotSpot reference frame (§13.2).
GPU_JAR=... bench-gpu/verify-frame.sh <cratonvm.exe> 7680 4320

# What the chunked overlap is worth, as a same-binary A/B (§13.5).
GPU_JAR=... bench-gpu/run-chunk-overlap.sh <cratonvm.exe> 1920 1440 5

# PTX and SASS branch counts, both arms of the if-conversion switch (§13.4).
GPU_JAR=... bench-gpu/count-kernel-branches.sh <cratonvm.exe> 640 480
```

and two for the GPULlama3 comparison the dispatch work in §13.5 is really
about — `run-gpullama3-ab.sh` (token rate) and `run-gpullama3-submit-ab.sh`
(host dispatch cost, with a per-round CPU control).

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
reached.

**§13 continues it one octave further still (1.85x at 11520×6480, §13.1)
and decomposes all four sizes with `GpuTransferFloor` (§13.6) — where the
amortising-fixed-cost explanation this section leans on turns out not to
survive: the transfer floor's own per-pixel cost RISES with n, which no
fixed term can produce.**

---

# 13. The residual pass, 2026-08-29

Everything §9 left open, taken to an answer. Two of the answers are that
the residual's own hypothesis was wrong; those are the interesting ones,
because a residual that is quietly dropped leaves the wrong belief behind.

Every number below was measured on the same box on the same afternoon, and
that box is shared: up to twenty-four `rustc` and `cargo` processes
belonging to other sessions came and went through the pass. Every
comparison here is therefore paired, arm-alternating, and reported as a
ratio or a per-round win count, and the absolute milliseconds are labelled
with whether their pass was quiet.

That is not a caveat, it is one of the findings. §13.6's decomposition took
four passes, and the first three each produced a confident wrong answer
with a mechanism ready to explain it. They are all written down there,
because the failure mode is not "noisy numbers" — it is plausible
structure, and a loaded box produces that as readily as a real effect.

## 13.1 A fourth resolution, 11520×6480 — the margin keeps shrinking

74,649,600 pixels, 2.25x the 8K point and 27x the largest size §7 ever
reached. `bench-gpu/run-raytracer-interleaved.sh`, 6 rounds, arm order
alternated, `XMX=10g` (the frame is an `int[]` of 298 MB and all three arms
allocate one), on a host gated quiet:

| round | CratonVM `--gpu` | TornadoVM PTX | ratio |
|---|---:|---:|---:|
| 1 | 35.08 | 60.83 | 1.73x |
| 2 | 29.15 | 56.50 | 1.94x |
| 3 | 33.43 | 60.58 | 1.81x |
| 4 | 34.69 | 61.45 | 1.77x |
| 5 | 29.20 | 60.91 | 2.09x |
| 6 | 35.30 | 63.97 | 1.81x |
| **mean** | **32.81** | **60.71** | **1.85x** |

CratonVM won 6/6. An earlier pass of the same six rounds against a loaded
box put both arms 30% higher (43.64 vs 84.55) and returned the same ratio to
within 5% — which is what pairing is for, and is the only thing about the
loaded pass worth keeping.

So the curve now reads **2.5x → 2.2x → 2.0x → 1.85x** across 2.76M, 8.29M,
33.2M and 74.6M pixels. §7 extrapolated a ~33% floor (1.5x) from the
per-pixel rates; four points later the margin is still above it and still
falling. It has not crossed.

## 13.2 8K and 11520×6480 are bit-identical to HotSpot

§12 measured timing only. `bench-gpu/verify-frame.sh` renders the same
frame on HotSpot and on CratonVM's `--gpu` path, dumps both, and diffs them
pixel by pixel:

```text
7680x4320    FRAMEDIFF n=33177600 differing=0 max_channel_delta=0 checksum_delta=0
             FRAMEDIFF verdict=BIT_IDENTICAL
11520x6480   FRAMEDIFF n=74649600 differing=0 max_channel_delta=0 checksum_delta=0
             FRAMEDIFF verdict=BIT_IDENTICAL
```

107.8 million pixels checked, none differing. §6's table now runs
unbroken from 160×120 to 11520×6480.

## 13.3 A bare scalar loop bound is accepted

`for (int i = 0; i < n; i++)` with `n` an `int` parameter. §9 called this
"worth doing; not done here", and named exactly what makes it different
from the `.length` case it sat next to: the launch grid is sized from the
largest array argument, so a bound larger than every array would
under-provision threads. A thread that is never created reaches no bounds
check, so the failure would be a silently short result rather than a
deopt — which is why the acceptance and the grid change are one change.

`emitter::WorkBound::ParamScalar` carries the parameter index to the
dispatch site, the marshaller records every `int` argument's value beside
the array lengths it already records, and the grid is sized from
`max(largest array length, that parameter)`. A scalar that did not reach
the marshaller refuses the launch rather than guessing. Only an unmodified
parameter qualifies — the host sizes the grid from the ARGUMENT while the
guard compares against whatever the local holds, and `n = n - 1` makes
those two different numbers. A 2-D rectangular nest bounded by two scalars
is still refused: its trip count is `rows * cols`, a product, and
`WorkBound` names one parameter.

`EligibleScalarBound.java` pins all four cases, and
`ptxas_round_trip_scalar_bounds` assembles the new guard shape — the only
one in the tree that reads a scalar `.param` where every other reads a
`_len`.

## 13.4 If-converting the branches makes this kernel SLOWER

This is the residual that came back with the opposite answer.

§9 read 18% of the kernel's SASS as branch machinery — 67 `BRA` plus 32
`BSSY`/`BSYNC`/`BMOV` triples, from short-circuit `&&`s and from ternaries
whose arms contain a call — and proposed if-converting the short,
side-effect-free ones into `selp`. That was implemented (see
`gpu/lowering-branches.md`), it engages, it is bit-exact, and it costs
time.

**It engages, and the census says exactly which half of it does what.**
`bench-gpu/count-kernel-branches.sh` dumps the kernel's PTX with
`CRATONVM_GPU_DUMP_PTX`, assembles it with the real `ptxas`, and counts
both levels. Three budgets, one binary:

| budget | PTX instr | `selp` | `bra` | SASS instr | `BRA` | `BSSY`/`BSYNC`/`BMOV` | branch machinery |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 (default) | 649 | 81 | 51 | 936 | 67 | 96 | **17.4%** |
| 8 | 615 | 92 | 29 | 920 | 66 | 93 | **17.3%** |
| unbounded | 603 | 96 | 21 | 904 | 58 | 81 | **15.4%** |

The 67 `BRA` and 96 reconvergence instructions at budget 0 reproduce §9's
count exactly.

Read the middle row against the bottom one and the whole answer is there.
**Budget 8 removes 22 PTX branches and 1 SASS branch.** The diamonds cheap
enough to be worth converting are ones `ptxas` was already converting by
itself, so doing it in the lowerer changes the PTX and not the machine
code — which is why budget 8 measures as a tie rather than a small win.
The 9 SASS branches that only fall at an unbounded budget are precisely
the ones `ptxas` declined to convert, and it declined for the same reason
the timing punishes converting them: they guard a square root.

**It is bit-exact.** Both arms, 1920×1440, against the HotSpot reference:
`differing=0`, `checksum_delta=0`.

**And it is slower.** The transfer floor is the control here — the same
launch shape and the same bytes out with no arithmetic, so
`CRATONVM_GPU_IF_CONVERT` cannot touch it — and eight interleaved rounds
at 1920×1440, minimum per arm:

| | best of 8 |
|---|---:|
| transfer floor (control) | 1.0062 ms |
| tracer, if-convert **off** | 1.1341 ms |
| tracer, if-convert **on** | 1.2056 ms |

The floor held between 1.0062 and 1.0399 across all eight rounds, so this
is not host drift; `on` was slower than `off` in **8 of 8**. Subtracting
the floor, the compute half goes **0.128 ms → 0.199 ms, 56% worse.**

The mechanism is in the kernel's own shape. A branch a warp does not
diverge on is nearly free — all 32 lanes skip the untaken arm together —
while `selp` makes every lane compute both. Four of this kernel's ternaries
are `disc > 0f ? (float) Math.sqrt(disc) : 1e9f`, one per sphere, and most
of a frame is background where a whole warp misses every sphere. Converted,
those warps compute four square roots they had been skipping, and
`sqrt.rn.f32` is a `MUFU.RSQ` plus a Newton-Raphson chain rather than one
instruction.

So the screen was counting the wrong thing. It capped an arm's LENGTH, and
the arms in question are one instruction each — a length cap cannot tell
`sqrt.rn.f32` from `mov.f32`. It now costs an arm instead, weighting
`sqrt`/`div`/`rcp`/`ex2` at 16 and everything else at 1, with the budget
tunable through `CRATONVM_GPU_IF_CONVERT_MAX_OPS` so the curve can be swept
in one binary rather than one build per point. Same host, same 1920×1440
frame, minimum of 5 rounds per setting, against the same transfer floor:

| budget | tracer | compute | vs off |
|---:|---:|---:|---:|
| 0 (off) | 1.1440 ms | 0.1305 ms | — |
| 2 | 1.1608 ms | 0.1473 ms | +13% |
| 4 | 1.1549 ms | 0.1414 ms | +8% |
| **8** | **1.1436 ms** | **0.1301 ms** | **−0.3%** |
| 16 | 1.1675 ms | 0.1540 ms | +18% |
| 32 | 1.2298 ms | 0.2163 ms | +66% |
| unbounded | 1.2244 ms | 0.2109 ms | +62% |

Floor 1.0135 ms. **The best budget is a tie with the feature switched off,**
and everything else is worse; the non-monotonicity between 2 and 8 is the
noise floor talking, since the whole compute half is 0.13 ms of a 1.14 ms
frame. And the census above says why a tie is the ceiling: at budget 8 the
lowerer is converting diamonds `ptxas` already converts, so it is rewriting
PTX that assembles to the same SASS.

**So the transform ships OPT-IN.** `CRATONVM_GPU_IF_CONVERT=1` turns it on
at budget 8, `CRATONVM_GPU_IF_CONVERT_MAX_OPS=<n>` at `n`, and unset does
nothing. It is kept, and kept reachable, rather than deleted: this is one
kernel, and a kernel with cheap arms and heavy divergence is exactly the
shape it was built for. The flags are how the next person measures whether
theirs is one — which is the whole point of the exercise, since this
residual sat in the record for eight days as an unpriced "would remove most
of that".

## 13.5 The per-launch event bookkeeping is real, and it is not this kernel's problem

§8d landed the chunked overlap at 1.51x where a raw prototype of the same
shape reached 1.67x, and attributed the gap to "the bridge's per-launch
`last_write` event bookkeeping for every device-pointer argument that the
raw prototype did not do". That bookkeeping has now been removed twice
over, and the gap did not move.

**What was removed.** Every kernel submission minted TWO `CUevent`s — one
inside `DeviceModule::launch_on_stream` for the per-buffer `last_write`
marker, one in `vm::runtime::offload` for the submission-completion marker
— and destroyed both on finalize. `DeviceContext` now keeps a capped free
list and `Event::drop` returns the handle to it; recycling is sound because
`cuStreamWaitEvent` captures an event's contents at the time of the call,
so a wait already issued cannot be reached back into by a later re-record.
Separately, every launch issued a `cuStreamWaitEvent` per device-pointer
argument, including the common case where that event was recorded on the
very stream about to launch — a driver round trip for an ordering the
stream already guarantees. `Event` now remembers which raw stream recorded
it, and both wait sites skip that case.

**The engagement census says both fire.** `cratonvm_types::gpu_event_census`,
printed under `CRATONVM_GPU_TIME_DISPATCH=1`, on a 32-token GPULlama3 run:

```text
[cratonvm] gpu events: created=15042 recycled=14178 (pool served 48.5%);
                       stream waits issued=0 elided=52512 (100.0% elided)
[cratonvm] gpu dispatch: calls=14610
```

Two events per dispatch, 29,220 in all, of which the pool served 48.5%.
And **every single one** of the 52,512 stream waits was a stream waiting on
itself.

**On the ray tracer it buys nothing measurable.** Same-binary A/B with
`CRATONVM_GPU_CHUNKS=1` as the kill switch, 1920x1440, 5 paired rounds
each:

| binary | chunks=1 | chunks=8 | overlap |
|---|---:|---:|---:|
| dev tip, before this work | 1.8135 ms | 1.2489 ms | **1.452x** |
| with the pool and the elision | 1.8494 ms | 1.2727 ms | **1.453x** |
| the same, on a quiet host | 1.6892 ms | 1.1558 ms | **1.462x** |

The third row is the one to read: five rounds spanning 1.441x to 1.493x,
a 3.6% spread, against a first pair taken while the box was busy. The
overlap is where it was.

That is the arithmetic, not a surprise: a frame is 8 chunk launches, so the
whole per-launch saving here is ~16 driver calls against a 1.25 ms frame —
a ceiling of two or three percent even if a driver call were free. **§8d's
attribution of the 1.51-vs-1.67 gap to event bookkeeping is not supported.**
What the remaining gap IS was not found by this pass; the page-locked
staging pool, the writeback into the Java heap, and the residency-cache
lookup are all still inside the difference and none of them has been
priced.

**Where it does pay is where launches are dense.** GPULlama3's forward pass
makes 453 submissions per token, not 8 per frame. Six interleaved rounds
with a per-round HotSpot CPU control, comparing median per-token
`submit_ms` (host time spent BUILDING the token's submissions): lower with
the pool and the elision in **6 of 6 rounds**, median per-round ratio
**0.886** — 11% off the host dispatch cost.

The lesson is the one §8d's own numbers already implied and the residual
mis-stated: this is a per-LAUNCH saving, so it is worth what the launch
rate makes it worth, and the ray tracer's launch rate is two orders of
magnitude below the workload where it matters.

## 13.6 The fit still holds past 8K — and a loaded box said three times that it did not

§12 asked whether "the same ~0.104 ms fixed / ~0.564 ns per-pixel fit still
holds, or whether something changes past 8.3M pixels". Nothing changes. Four
sizes spanning a 27x range, `bench-gpu/run-transfer-decomposition.sh` running
`GpuTransferFloor` (same launch shape, same bytes out, no arithmetic) and
`RayTracerKernel` alternately within each size, minimum per arm across
rounds, on a host gated quiet by `bench-gpu/wait-for-quiet.sh`:

| pixels | floor | tracer | floor ns/px | tracer ns/px | **compute ns/px** |
|---:|---:|---:|---:|---:|---:|
| 2,764,800 | 1.0015 ms | 1.0828 ms | 0.362 | 0.392 | **0.029** |
| 8,294,400 | 2.9939 ms | 3.1862 ms | 0.361 | 0.384 | **0.023** |
| 33,177,600 | 12.5718 ms | 13.1611 ms | 0.379 | 0.397 | **0.018** |
| 74,649,600 | 31.4237 ms | 33.3750 ms | 0.421 | 0.447 | **0.026** |

**The compute half — the quantity this decomposition exists to isolate — is
flat at 0.018 to 0.029 ns/px over a 27x range in n.** After §8c collapsed
the double-precision square roots, this kernel is a transfer with some
arithmetic attached, and nothing about that changes at 4K, at 8K, or an
octave past 8K. §12's shrinking vs-TornadoVM margin is therefore a transfer
story, and the thing to profile next is the writeback path, not the kernel.

The transfer floor's own per-pixel figure is quoted with less confidence
than the compute column. A second quiet pass over the same four sizes read
0.354 / 0.356 / 0.345 / 0.355 ns/px — flat where the table above rises 16% —
so the floor is not resolved here to better than about 18% at the large
sizes, and any trend inside that band is not a finding. (It is also why the
least-squares fit over these four points returns a small NEGATIVE intercept:
a slope that wanders by 16% across the range makes a two-parameter fit
report the wander rather than a fixed cost. The per-size numbers are the
measurement; the fit is not.)

### The three answers this produced before it produced the right one

That table took four passes, and the first three each produced a confident
wrong finding. They are worth writing down, because the failure mode is not
"noisy numbers" — it is *plausible structure*:

1. **Mean over rounds, loaded box.** One loaded tracer round put a tracer
   reading BELOW its own floor, and the fit returned a negative fixed cost
   with a straight face.
2. **Minimum over rounds, loaded box.** No more impossibilities, and a clean
   monotone story instead: the floor's per-pixel cost "rises 40% with n",
   0.340 to 0.475 ns/px. It is the kind of finding that gets written up —
   PCIe queueing, device-memory pressure, a page-locked slab past some
   threshold. It is not there.
3. **Minimum over rounds, quiet until the last size.** The floor came out
   flat and the tracer tracked it to 33.2M, then added 13.7 ms at 74.6M — a
   sharp, localised cliff at exactly one size, which is the most convincing
   shape of all. The chunk A/B settles it: quiet, `chunks=8` at that size is
   30.89 ms, and the 40.24 ms the loaded pass recorded is almost exactly its
   own `chunks=1` number. The loaded pass had measured the un-overlapped
   path.

Each of those had a mechanism ready for it. None of them was the mechanism.
`wait-for-quiet.sh` exists because of the third one.

### And the overlap is alive at every size

The cliff hypothesis was worth testing on its own terms, since serial
dispatch is `kernel + transfer` where overlapped is `max(kernel, transfer)`
— and a floor whose kernel is ~0 cannot tell the two apart, which is exactly
why it would stay linear while the tracer did not. Same-binary A/B on
`CRATONVM_GPU_CHUNKS`, quiet, paired rounds:

| pixels | `chunks=1` | `chunks=8` | overlap |
|---:|---:|---:|---:|
| 2,764,800 | 1.6892 ms | 1.1558 ms | **1.462x** |
| 8,294,400 | 4.6381 ms | 3.7354 ms | **1.242x** |
| 33,177,600 | 17.3413 ms | 13.6579 ms | **1.270x** |
| 74,649,600 | 42.4034 ms | 30.8858 ms | **1.373x** |

It never stops working. The 1.462x at 1920×1440 also reproduces §8d's 1.51x
at the same size to within the 3.6% spread of its own five rounds — which,
for a number first measured eight days and several hundred commits earlier,
is the closest thing this record has to a control.

## 13.7 Where the CPU path's remaining 5.3x goes: mostly the vectorizer

§8b took the CratonVM CPU path from ~11x HotSpot to ~5.3x on this kernel by
intrinsifying `Math.min(float,float)`, and left "where the remaining 5.3x
goes is unprofiled".

A whole-kernel ratio cannot answer that — the kernel is ~40 float ops, 8
square roots, 12 divisions, a dozen selects and one packed store per pixel,
and any one of them could carry the factor.
`bench-gpu/RayTracerCpuAblation.java` isolates each construct the kernel
actually contains, over the same arrays at the same length (2,764,800) with
the same loop shape, and runs it on both VMs. Best of 12, ns per element:

| construct | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| `out[i] = a[i] + b[i]` (memory floor) | 1.399 | 4.614 | **3.30x** |
| 8 chained `x*k + y` | 1.466 | 30.617 | **20.89x** |
| 4 x `(float) Math.sqrt` | 3.005 | 22.497 | 7.49x |
| 6 x `Math.min(float,float)` | 7.535 | 26.399 | **3.50x** |
| 4 compare-and-select ternaries | 3.802 | 19.471 | 5.12x |
| 4 float divisions | 2.546 | 15.845 | 6.22x |
| clamp + `f2i` + shifts + `int` store | 1.664 | 12.761 | 7.67x |

Read the two extremes together and the answer falls out. The multiply-add
chain is 20.9x — and on HotSpot it costs 1.47 ns/element, which is barely
above the 1.40 ns/element of a loop that does no arithmetic at all. HotSpot
is not computing those eight multiply-adds faster than CratonVM; it is
computing eight of them per lane of a vector register while CratonVM
computes one. At the other end, `Math.min(float,float)` is 3.50x — and it
is the one construct HotSpot cannot vectorise, because Java's NaN and
signed-zero rules are not what `MINPS` implements (the same rules §8b had
to reproduce by hand for the scalar intrinsic). The bare memory floor,
which HotSpot's vectoriser also cannot help much, is 3.30x.

So the profile reads:

* **CratonVM's scalar float code is ~3.3-3.5x HotSpot's.** That is the
  floor, visible wherever the semantics switch HotSpot's vectoriser off.
* **The rest, up to 21x on the friendliest shape, is auto-vectorisation
  CratonVM does not do.** Nothing in the list is a single slow construct of
  the kind `Math.min` was in §8b; there is no second `Math.min` to find
  here.

That the whole-kernel ratio is 5.3x rather than 20x is consistent with
both: the real kernel's per-pixel branches and dependent chains stop
HotSpot's vectoriser doing to it what it does to the ablation's cleanest
loop, so the gap it opens on the real thing sits nearer the scalar floor.

## 13.8 TornadoVM's own divergence: not ours, and already named as far as it can be

§9 carried "TornadoVM's own GPU-vs-Java divergence (§6) is reported, not
diagnosed". §6 does in fact name the mechanism at instruction level, from
`tornado --printKernel`: 42 `mad.rn.f32` (a fused multiply-add where the
JLS requires two roundings — the same defect this record found and fixed in
CratonVM's own lowerer, §2), 12 `div.full.f32` (the ~2 ULP approximate
divide where Java requires correctly-rounded, and where CratonVM emits
`div.rn.f32`), and `min.f32` (which implements neither Java's NaN rule nor
its signed-zero rule — the same two rules §8b had to hand-build).

What is left is attributing those to lines of a third-party backend's
compiler, which is TornadoVM's to do and is not diagnosable from outside
it. Reconfirmed at 11520x6480 in §13.1: TornadoVM's checksum is
196463340438801 where CratonVM's and HotSpot's agree at 196463343925950.

**Closed as out of scope.** The correctness claim this record makes is
about CratonVM's output against HotSpot, and that claim is now verified
bit-for-bit at every resolution from 160x120 to 11520x6480. The TornadoVM
column is a throughput comparison against a computation that produces a
different answer, and every table here says so.
