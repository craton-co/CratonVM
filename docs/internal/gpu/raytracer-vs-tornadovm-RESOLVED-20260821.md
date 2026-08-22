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
measured at one resolution, and 640x480 happens to sit just below the
crossover. Best-of-20 wall time, full per-call round trip:

| n (pixels) | HotSpot CPU | CratonVM CPU | CratonVM `--gpu` | TornadoVM PTX | Craton/Tornado |
|---:|---:|---:|---:|---:|---:|
| 19,200 | 0.617 ms | 9.78 ms | **0.080 ms** | 0.270 ms | **3.37x** |
| 76,800 | 2.130 ms | 26.21 ms | **0.163 ms** | 0.397 ms | **2.43x** |
| 307,200 | 8.250 ms | 90.60 ms | **0.444 ms** | 0.614 ms | **1.38x** |
| 1,228,800 | 31.76 ms | 349.09 ms | 1.395 ms | **1.359 ms** | 0.97x |
| 2,764,800 | 70.86 ms | 782.11 ms | 2.958 ms | **2.744 ms** | 0.93x |

Fitting the top two rows separates fixed cost from per-pixel cost:

| | fixed per call | per pixel |
|---|---:|---:|
| CratonVM `--gpu` | **0.144 ms** | 1.018 ns |
| TornadoVM PTX | 0.250 ms | **0.902 ns** |

So the honest statement is: **CratonVM's per-launch overhead is ~1.7x
lower and its per-pixel kernel cost is ~13% higher**, and the two cross
at roughly n = 970,000. A single-resolution measurement cannot tell those
apart, which is why the original 1.3-1.4x claim was not wrong so much as
undecomposed.

CratonVM's CPU path is a separate matter: 11x slower than HotSpot on this
kernel at every size, flat across the sweep. That is not this record's
subject and is left open (§9).

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

**Wall-clock on this kernel: unchanged.** That is the honest and slightly
disappointing result — halving the instruction count moved the 1920x1440
number from 2.958 ms to within noise of itself, which says this kernel is
not instruction-bound at these sizes. What the change does buy is the
first-call latency (`ptxas` compile is part of CratonVM's fixed cost, the
metric it already wins on), register headroom, and a real improvement for
any kernel that *is* instruction-bound. It is kept on those grounds, not
on this benchmark's.

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
* **The CratonVM CPU path is ~11x HotSpot on this kernel**, flat across
  the sweep (§7). Unprofiled. One concrete lead: `Math.min`/`Math.max`
  are JIT intrinsics for `(II)I` and `(JJ)J` only — the `(FF)F` and
  `(DD)D` forms fall through to the JDK's Java implementation, which for
  floats contains a nested `Float.floatToRawIntBits` call and a
  `getstatic`. This kernel calls `Math.min(float,float)` three times per
  pixel. Not measured, so not claimed.
* TornadoVM's own GPU-vs-Java divergence (§6) is reported, not diagnosed.

## 10. Reproduction

```sh
# One command, all four paths, five resolutions, with a FrameDiff verdict
# per row against the HotSpot reference frame.
PYTHON3_DIR=<dir containing python3.exe> \
  bench-gpu/run-raytracer-comparison.sh results.md
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
