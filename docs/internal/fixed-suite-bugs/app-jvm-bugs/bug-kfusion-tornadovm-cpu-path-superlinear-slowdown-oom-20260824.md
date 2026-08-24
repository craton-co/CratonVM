# KFusion-TornadoVM CPU path: super-linear per-frame slowdown, OOM abort by frame 13

## Status

Open, not yet root-caused. Reproduced once on `dev` HEAD `3ed73bf89` (rebuilt into an
isolated `target-retest/` dir, see [bug-gpullama3-unsafe-getshort-model-load-livelock.md](bug-gpullama3-unsafe-getshort-model-load-livelock.md)
for why an isolated build dir was used this session).

## Context

`apps/kfusion-tornadovm` (a fork of [beehive-lab/kfusion-tornadovm](https://github.com/beehive-lab/kfusion-tornadovm))
implements KinectFusion in Java. The real GPU/TornadoVM-accelerated path
(`kfusion.tornado.Benchmark`) needs an actual TornadoVM runtime with a GPU/OpenCL/CUDA
backend, which isn't set up here. `kfusion.java.Benchmark` is the pure-CPU mirror: it
uses the same `uk.ac.manchester.tornado.api.types.*` data structures but runs everything
as plain Java (`Renderer.render`, `renderWithParallelStreams`, `IterativeClosestPoint`,
`VolumeOps`), so it's runnable under CratonVM without any TornadoVM/GPU dependency.

The app's own `target/` already had a full `mvn install` build from earlier this session
(target/classes + target/kfusion-tornadovm-0.1.0.jar + all deps, including a real
resolved `tornado-api-5.2.1-jdk25` — the pom pins `tornadovm.version` per-JDK, so the
"unresolvable ancient tornado-api:0.14-dev" problem mentioned in earlier session notes
turned out to already be solved by that pin; no stand-in TornadoVM API classes were
needed after all).

## Getting a real dataset (scene2raw doesn't exist upstream)

`downloadDataSets.sh` expects a tool at `slambench/build/kfusion/thirdparty/scene2raw`
that converts the ICL-NUIM dataset into the `.raw` format `RawDevice.java` reads. That
tool does not exist in any reachable form:

- Not in the current `pamela-project/slambench` (`update-master` branch) — that repo was
  restructured into a multi-algorithm "SLAMBench2" framework; `kfusion` is now a
  separately-downloaded algorithm module (`make kfusion`) that only builds as a `.so`
  loaded by slambench's own `-load` mechanism, with no standalone dataset tooling.
- Not in the current `pamela-project/kfusion` (what `make kfusion` actually clones).
- Not in the original `GerhardR/kfusion` (a live-camera CUDA demo, no ICL-NUIM tooling).
- No tags/releases/alternate branches on `pamela-project/slambench` preserve the old
  pre-2017 layout `downloadDataSets.sh` was written against.

Building slambench's C++ framework from scratch (to confirm none of this had quietly
grown a converter) was itself a multi-hour GCC15/CMake4.2-vs-2013-era-code exercise —
opencv 3.4.3, eigen3, suitesparse, cvd, flann, pangolin, pcl, sophus, the slambench
framework, and the `kfusion` algorithm module were all patched to build clean (missing
`<cstdint>`/`<limits>`/`<fstream>`/`<cstdio>`/`<stdexcept>` includes, Boost API renames,
`cmake_policy` removals, GCC 13+ `-Wtemplate-body`, and a GCC15-default-C++20 vs.
Eigen's `array == array` idiom under `-Werror=deprecated-copy`/rewritten-comparison
rules — see git history around 2026-08-23/24 for the exact patch sequence if this needs
repeating elsewhere). None of it produced `scene2raw`; the framework build was a dead
end for this specific purpose.

**Fix**: wrote a replacement converter (`scene2raw.py`, ~70 lines, numpy+Pillow) reading
directly from the ICL-NUIM `living_room_traj2_loop.tgz` tarball's ASCII `.depth` (Euclidean
ray-length, meters) and `.png` (RGB) members, converting ray-length to perpendicular
Z-depth via `Z = L / sqrt(1 + ((u-cx)/fx)^2 + ((v-cy)/fy)^2)` (standard ICL-NUIM
formula) with the app's own intrinsics (`fx=481.2, fy=480, cx=320, cy=240`, from
`RawDevice.CAMERA`/`Benchmark.main`), then millimeter-rounds to `uint16`. Output format
reverse-engineered from `RawDevice.java` directly: per frame, an 8-byte ignored header +
`width*height` depth `uint16`s (LE) + an 8-byte ignored header + `width*height` RGB
byte-triples — matches `calcFrameSize() = 16 + w*h*5` exactly. Converted the first 50 of
882 available frames (enough to exercise steady-state tracking/integration/raycasting,
not a full-trajectory run). Verified by running the *real* KinectFusion CPU pipeline
against it under HotSpot first (see below) — ICP tracking produced small, monotonically
evolving pose deltas frame-to-frame, confirming the depth conversion is sane and not
just structurally well-formed noise.

Config used: a copy of `conf/bm-traj2.settings` (`conf/bm-traj2-local.settings`) with
`kfusion.raw.file` pointed directly at the converted `.raw` file's absolute path (bypasses
`RawDevice`'s `http:`-prefixed auto-download branch entirely — no `$HOME`/`~/.kfusion_tornado`
indirection needed).

## Repro

```
cd apps/kfusion-tornadovm
CP="target/classes:target/*.jar"   # (built via one `mvn clean install -DskipTests` already; ; on Windows)

# HotSpot baseline
java -Xms4G -cp "$CP" kfusion.java.Benchmark conf/bm-traj2-local.settings

# CratonVM
cratonvm.exe --java-home <jdk25> -Xms4G -cp "$CP" kfusion.java.Benchmark conf/bm-traj2-local.settings
```

## HotSpot baseline: flat, ~4-16 fps, completes all 50 frames

```
frame  ...  total     X          Y          Z          tracked  integrated
0      ...  0.954390  0.000000   0.000000   0.000000   0        1
1      ...  0.726922  0.000000   0.000000   0.000000   0        1
4      ...  0.836706  -0.004366  0.001435   0.000934   1        1
8      ...  0.688738  -0.006291  0.000104   0.000240   1        1
16     ...  1.039402  -0.014927  -0.001206  -0.001111  1        1
24     ...  1.235164  -0.038573  0.000087   -0.004031  1        1
32     ...  1.210658  -0.089924  -0.000256  -0.010808  1        1
40     ...  1.269650  -0.172732  -0.005572  -0.014573  1        1
49     ...  0.413567  -0.243110  -0.002848  -0.000819  1        0
Summary: time=36.73, frames=50, FPS=1.36
```
Per-frame total drifts up mildly (0.95s -> ~1.2s) as the fused volume gets denser —
expected, and stays in the same order of magnitude for all 50 frames.

## CratonVM: super-linear growth, OOM abort at frame 13

```
frame  ...  tracking    raycasting   total        X          Y          Z          tracked  integrated
0      ...  20.576957   0.000015     120.608777   0.000000   0.000000   0.000000   0        1
4      ...  103.646394  149.368658   344.565786   -0.004366  0.001435   0.000934   1        1
8      ...  102.930279  146.898949   346.507312   -0.006291  0.000104   0.000240   1        1
10     ...  87.733710   212.274895   377.711165   -0.006941  0.000429   0.001525   1        1
11     ...  413.694197  1193.851892  1622.149913  -0.006791  0.000041   0.000718   1        0
12     ...  801.603238  811.046791   2191.715543  -0.010145  -0.000093  0.001272   1        1
```
then:
```
[WARN cratonvm_jit::x64::driver] JIT compile bailed: code buffer estimate too small; retrying
  method=uk/ac/manchester/tornado/api/types/utils/VolumeOps.grad(...)
memory allocation of 536870912 bytes failed
```
process aborts (Rust allocator abort on a failed 512MB request), exit code reported as
127 by the wrapping shell.

Frame 0 alone is ~126x slower than HotSpot's frame 0 (120.6s vs 0.95s) — already far
beyond a flat interpreter/no-JIT-warmup tax. `tracking` and `raycasting` are the columns
that blow up (20.6s -> 801.6s and ~0s -> 811.0s respectively over just 12 frames);
`acquisition`/`preprocessing` (I/O-bound, no volume interaction) stay flat and roughly
comparable to HotSpot throughout, which points at something volume-state-dependent
rather than a general interpreter/dispatch slowdown affecting every code path equally.
`IterativeClosestPoint.reduce` and `VolumeOps.grad` both hit the JIT's
"code buffer estimate too small, retrying at measured size" path during this run — unclear
yet whether that's contributing to the growth or just visible near it in the log.

## Not yet done

- Root cause: is this a real per-frame algorithmic cost increase specific to
  CratonVM's execution of these particular hot loops (interpreter fallback repeatedly
  invalidating an inline cache, a JIT recompile loop, GC pressure from the volume's
  growing live set), or a leak that happens to correlate with frame count? The
  `tracking`/`raycasting`-only blowup (vs. flat `acquisition`/`preprocessing`) is the one
  concrete lead so far.
- What exactly requests the 512MB single allocation that aborts — not yet traced.
- Whether this reproduces on `--nojit`, and whether it reproduces at all frame counts or
  only once the volume has integrated enough surface to make `VolumeOps.grad`/ICP's
  reduction expensive.
- A full 882-frame run (not attempted — the 50-frame partial dataset already crashes by
  frame 13).

## Related files

- [apps/kfusion-tornadovm/src/main/java/kfusion/java/Benchmark.java](../../../../apps/kfusion-tornadovm/src/main/java/kfusion/java/Benchmark.java)
- [apps/kfusion-tornadovm/src/main/java/kfusion/java/devices/RawDevice.java](../../../../apps/kfusion-tornadovm/src/main/java/kfusion/java/devices/RawDevice.java)
- `apps/kfusion-tornadovm/src/main/java/kfusion/java/algorithms/IterativeClosestPoint.java`
- `uk/ac/manchester/tornado/api/types/utils/VolumeOps.java` (in `tornado-api-5.2.1-jdk25.jar`)
