# Ray tracer kernel: CratonVM GPU offload vs TornadoVM, RTX 2060 — 2026-08-21

**What this is:** a simplified twin of `apps/TornadoVM-Ray-Tracer` (the real
app was too complex for either analyzer today — see "What was cut" below),
run three ways on the same RTX 2060: HotSpot CPU, CratonVM `--gpu-driver`,
and TornadoVM's PTX backend. **Headline: CratonVM's GPU offload is ~1.3-1.4x
faster than TornadoVM for this kernel in steady state, and both are far
ahead of CPU** — but there's an open, unexplained checksum divergence at
scale that needs to be resolved before trusting this as a clean result.

## The kernel

Four fixed spheres (unrolled, no scene loop), branchless closest-hit
selection (no `if` in the reduction — matches both analyzers' "no general
control flow inside a kernel" limitation), single diffuse term against a
fixed light direction, orthographic camera. One thread per pixel.

Two files, same math, adapted to each toolchain's constraints:
- `bench-gpu/RayTracerKernel.java` — CratonVM twin. `@GpuKernel(admit =
  AdmissionHint.ALLOW_INTRINSIC_CALLS)` (needed for `Math.sqrt`/`Math.min`).
  Loop bound must trace to an array's cached `.length` local — a bare scalar
  parameter or `out.length` inline in the condition both got rejected by the
  lowerer (`UnsupportedNode`) before this was found.
- `bench-tornado/RayTracerTornado.java` — TornadoVM twin. The 20 sphere
  scalars had to be bundled into one `FloatArray` — `TaskGraph.task()` caps
  at 15 arguments and the naive per-scalar signature (25 params) doesn't
  compile against any overload.

## What was cut from the real app

The real `RayTracer.java`/`BodyOps.java` do reflections (recursive ray
bounces), soft shadows, a plane, and a skybox — all need either a
dynamic-length scene loop or per-pixel recursion, and neither analyzer
admits general recursion or arbitrary-length loops inside a kernel today.
This twin is a primary-ray-only, no-shadow, no-reflection reduction: real
"ray tracing" in the sense of ray-sphere intersection + diffuse shading, but
a small fraction of the original's per-pixel work.

## Results — 640x480 (307,200 pixels), 10 iterations after 1 warm-up

| Path | mean | best | vs HotSpot CPU |
|---|---:|---:|---:|
| HotSpot CPU (interpreter+C2) | 8.594 ms | 7.901 ms | 1x |
| CratonVM CPU (`--nojit` not set, JIT on, no `--gpu`) | 93.648 ms | 90.834 ms | 0.09x (slower — known interpreter/JIT overhead, not this record's subject) |
| **CratonVM `--gpu-driver`** | **0.461 ms** | **0.428 ms** | **18.6x** |
| **TornadoVM (PTX backend)** | **0.666 ms** | **0.562 ms** | **12.9x** |

CratonVM's GPU path here is ~1.3-1.4x TornadoVM's throughput on the same
device for the same computation. Take this as one data point, one kernel,
one run — not a general claim.

## Open problem: checksum divergence at scale — NOT resolved

Sum-of-output-pixels checksums:

| Path | 8x8 (n=64) | 640x480 (n=307,200) |
|---|---:|---:|
| HotSpot CPU | 1073741760 | 3960240612783 |
| CratonVM CPU (interpreted) | 1073741760 | 3960240612783 |
| CratonVM GPU | 1073741760 | 3960181333290 |
| TornadoVM GPU | 1073741760 | 3837845302746 |

All four agree exactly at 8x8. **Both GPU paths diverge from the CPU/HotSpot
reference at 640x480, and diverge from each other.** This was not
root-caused this session. The leading hypothesis is float-rounding at
sphere-boundary pixels (the branchless `best == hit0/1/2/3` mask comparison
and the `yy0 < sr0` bounds check are both exact-equality/threshold tests on
computed floats, which can tip either way under GPU vs CPU rounding for
pixels near a boundary) rather than a structural logic bug — small-scale
matching supports this over a wholesale kernel error, but the divergence
was not measured for MAGNITUDE (how many pixels differ, by how much) or
isolated to a specific pixel/sphere pair. TornadoVM's own divergence from
CratonVM's GPU result (not just from CPU) means this isn't simply "one
VM's GPU path has a bug" — both differ from the reference and from each
other.

**Do not quote the speedup numbers above as validated correctness-clean
until this is resolved.** The `docs/gpu/README.md` status line claims
"Checksums match HotSpot bit-for-bit on every kernel tested" for CratonVM's
existing fixture set (vector-add, dot-reduction, etc.) — this kernel is
more complex (a branchless 4-way reduction with float equality
comparisons) than anything in that set, and is the first one found to
diverge.

## Next steps

1. Diff the two GPU outputs against the CPU reference pixel-by-pixel at
   640x480 to find how many pixels differ and whether they cluster at
   sphere silhouette edges (supports the rounding hypothesis) or are
   scattered/systematic (would refute it).
2. Re-run with `CRATONVM_GPU_NO_ZEROCOPY=1` to rule out the zero-copy DMA
   marshalling path specifically.
3. Try a version of the branchless selection using a small epsilon-tolerant
   comparison instead of exact float equality for the `m0..m3` masks, and
   see if the divergence shrinks or disappears.
4. Once resolved, re-run at a couple of other resolutions to see if the
   speedup ratio holds or is resolution-dependent (a fixed per-launch
   overhead would show relatively worse at small n, better at large n, for
   both GPU paths).

## Reproduction

```sh
# CratonVM (gpu-driver build required — see scripts/build-gpu.bat)
javac -cp C:/craton/gpu-java/target/craton-gpu-0.2.0.jar -d bench-gpu bench-gpu/RayTracerKernel.java
target-gpu/release/cratonvm.exe --java-home <jdk25> --gpu --gpu-min-work 1 -cp bench-gpu RayTracerKernel 640 480 10

# TornadoVM (needs a real python3 on PATH -- the `tornado` launcher execs
# python3 to assemble its module-path/classpath; this box's `python3` alias
# was the broken Microsoft Store stub, not C:/Python314/python.exe)
source /c/craton/tornadovm/setvars.sh
tornado --classpath bench-tornado RayTracerTornado 640 480 10
```
