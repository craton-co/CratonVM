# Ray tracer kernel: CratonVM GPU offload vs TornadoVM, RTX 2060

**Status: RESOLVED 2026-08-21.** The checksum divergence this record opened
on was a real CratonVM defect, is root-caused, is fixed, and the fix is
pinned by a regression test. CratonVM's GPU output is now bit-identical to
HotSpot on every pixel at every resolution measured. Two further defects
found on the way — one in each benchmark twin — are also fixed. The
headline speedup claim the original record made **did not survive**
measurement across a resolution sweep and has been replaced with the
decomposition below.

Superseded documents: the original open record at
`bench-gpu/results/raytracer-vs-tornadovm-20260821.md`.

Durable documentation this produced:
[`docs/gpu/README.md` -> "Float bit-exactness"](../../gpu/README.md#float-bit-exactness).

---

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

## 7. Performance result — the original headline does not hold

The record's "~1.3-1.4x faster than TornadoVM in steady state" was
measured at one resolution. It is not a steady-state figure: it is the
value the ratio happens to take at 640x480, on a curve that falls
monotonically with problem size.

`bench-gpu/run-raytracer-interleaved.sh` measures the two GPU arms
**paired** — alternating which runs first within each round, six rounds,
best-of-30 each — because this box carries a variable background CPU load
and both arms spend real host time marshalling and launching. Two
unpaired sweeps forty minutes apart disagreed about which arm was faster
at 1920x1440, which is what motivated pairing.

| n (pixels) | CratonVM `--gpu` | TornadoVM PTX | CratonVM ahead by | rounds won |
|---:|---:|---:|---:|---:|
| 307,200 | **0.418 ms** | 0.675 ms | **38.2%** | 6/6 |
| 1,228,800 | **1.311 ms** | 1.496 ms | **12.4%** | 6/6 |
| 2,764,800 | **2.711 ms** | 2.821 ms | 3.9% | 4/6 |

CratonVM is ahead everywhere measured, but **the margin collapses
monotonically with n**, and by 1920x1440 it is inside round-to-round
noise — two of six rounds go the other way. Fitting the two ends
separates why:

| | fixed per call | per pixel |
|---|---:|---:|
| CratonVM `--gpu` | **0.191 ms** | 0.911 ns |
| TornadoVM PTX | 0.436 ms | **0.863 ns** |

**CratonVM's per-launch overhead is ~2.3x lower; its per-pixel kernel
cost is ~6% higher.** Extrapolated, the two cross somewhere above five
million pixels. A single-resolution measurement cannot tell those two
facts apart, which is why the original 1.3-1.4x claim was not so much
wrong as undecomposed — and why quoting any one ratio as "the speedup"
for this pair is the wrong shape of claim to make.

For the same reason the CPU columns are omitted from the table above:
they are not paired, and the HotSpot control drifted between 8.2 ms and
14 ms at 640x480 across this session depending on what else the box was
doing. The GPU arms barely move under that load (they are GPU-bound),
which is exactly why they can be compared to each other and not to
numbers from another day. The interleaved script prints the control
alongside each round and flags it, rather than silently averaging a
loaded round in.

CratonVM's CPU path is a separate matter: roughly 11x slower than HotSpot
on this kernel, and flat in that ratio across the whole sweep — 9.8 ms vs
0.62 ms at 19,200 pixels, 782 ms vs 71 ms at 2,764,800. A constant ratio
across a 144x range in n says this is per-iteration work, not a fixed
cost or a scaling cliff. Not this record's subject; left open in §9 with
the one concrete lead found.

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
```

The script exists because three separate footguns turn this measurement
into a plausible-looking lie: `java.exe` silently produces no output when
handed an MSYS `/c/...` classpath, TornadoVM silently runs on the CPU
when its compiler bails out, and its launcher needs a `python3` on PATH
that a Windows Python install does not provide. Each is handled and each
is commented at the point it is handled.

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
