# KFusion-TornadoVM CPU path: ~126x slower than HotSpot, growing per frame

## Status

**Root-caused and measured.** One contributing JIT defect is FIXED here (the
code-buffer estimate ignored nested inline bodies). The DOMINANT cause — FFM
`MemorySegment` element access costing ~1.1µs against a `short[]`'s ~1ns — is
diagnosed, priced, and **not fixed**: closing it needs a JIT intrinsic, scoped
at the end of this page. Do not read this page as "kfusion is fixed"; it is not.

An earlier revision of this page mis-attributed the per-frame columns (it read
column 4 as `raycasting`; the header is
`frame acquisition preprocessing tracking integration raycasting rendering
computation total X Y Z tracked integrated`). Every number below is re-measured
against the correct columns.

## What the app is

`apps/kfusion-tornadovm`'s `kfusion.java.Benchmark` is the pure-CPU mirror of
the TornadoVM GPU pipeline: same `uk.ac.manchester.tornado.api.types.*` data
structures, but every stage runs as plain Java, so it needs no GPU/OpenCL
backend. The TSDF volume is a `VolumeShort2` backed by a
`uk.ac.manchester.tornado.api.types.arrays.ShortArray`, and **that is the whole
story**: `ShortArray` is backed by a Panama `MemorySegment`, so every voxel read
and write is a `MemorySegment.getAtIndex`/`setAtIndex` call.

```
VolumeShort2.get(x,y,z) -> ShortArray.get(i) -> TornadoMemorySegment.getShortAtIndex(i)
                        -> MemorySegment.getAtIndex(ValueLayout.JAVA_SHORT, i)
```

## Measured: the per-element cost

Microbenchmark (`SegBench2`/`UnsafeBench`, 2M iterations, steady state). The
`Unsafe.getShort(long)` column is the control: a native that is about as lean as
a native can be, reached through the SAME dispatch funnel.

| access | CratonVM | HotSpot |
|---|---|---|
| `short[]` element | 0.8 ns | ~1 ns |
| `Unsafe.getShort(long)` | **303 ns** | (intrinsified) |
| `MemorySegment.getAtIndex(JAVA_SHORT, i)` | **1158 ns** | ~1 ns |

Two separate findings, and the second is the one that matters:

1. **A native call costs ~300 ns.** `Unsafe.getShort` does almost nothing beyond
   the dispatch, so ~300 ns is the funnel floor. `bytecode_walk.rs` already
   records this independently ("replaces a full native dispatch (~250 ns/op
   measured)" on the `AtomicInteger` intrinsic).
2. **The segment accessor adds ~850 ns on top of the funnel.** A staged bisect
   of `pe_segment_get_at_index` (temporary `CRATONVM_SEGPROBE` early-returns,
   normalised against the in-run `byteSize` control because the host was
   contended by another session's builds) attributes it:

   | stage | added work | cost |
   |---|---|---|
   | 1→2 | argument unpacking | ~0 |
   | 2→3 | layout classification | +1.4 funnel-units |
   | 3→4 | heap-vs-native shape probe | ~0 |
   | 4→6 | **`pe_segment_access_addr`** (scope liveness + address + size + bounds) | **+4 funnel-units** |
   | 6→0 | the actual load | ~0 |

   `pe_segment_access_addr` is ~10 `ctx` round-trips per element —
   `pe_segment_check_scope` walks segment→arena→session and resolves the session
   slot map, then `segment_address` and `segment_byte_size` each probe the
   segment's shape — and each round-trip pays heap validation/forwarding.

**Caching those probes is not the fix.** Memoizing the segment shape, the field
indices and the session slot map per class-id was implemented and measured
against the untouched `Unsafe.getShort` control: `segment/Unsafe` went 3.82 →
3.53, i.e. **~8%**. That was reverted rather than shipped — 8% does not justify
adding class-redefinition staleness to a capability gate and to the FFM
correctness path, and the target needs ~100x, not 8%.

## Why the per-frame time GROWS

Not a leak. `Raycast.raycast` calls the expensive normal only on a ray that HITS
a surface:

```java
if (hit.getW() > 0f) {
    final Float3 surfNorm = VolumeOps.grad(volume, volumeDims, position);
```

`VolumeOps.grad` is 1511 bytes with 273 invokes and does ~8 trilinear `vs()`
probes — i.e. dozens of segment reads per hit. As frames integrate, more of the
volume holds surface, so more rays hit, so `grad` runs for more pixels. On
HotSpot that growth is invisible (each read is ~1 ns); here each read is ~1.1µs,
so the same growth is catastrophic.

Measured, `dev` HEAD `589cead9d`, 50-frame converted ICL-NUIM traj2:

| frame | total | integration | tracking | raycasting |
|---|---|---|---|---|
| 0 | 95.9 s | 57.1 | 17.6 | 0.00 |
| 1 | 80.9 s | 55.0 | 17.5 | 0.00 |
| 2 | 79.9 s | 57.8 | 14.8 | 0.00 |
| 3 | 228.6 s | 84.3 | 16.1 | **119.9** |
| 4 | 313.2 s | 50.7 | 97.9 | **139.6** |
| 5 | 378.5 s | 0.0 | 148.2 | **215.9** |
| 6 | 500.5 s | 112.8 | 153.4 | **220.7** |

HotSpot runs the same 50 frames at 0.95→1.2 s/frame, flat.

The flat ~80 s floor is `integration`: it sweeps the whole 256³ volume every
frame, ~16.7M voxels × (a read + a write) × ~1.1µs ≈ 37 s, which is the measured
57 s once the surrounding `Short2` allocation is included. The growth on top is
`raycasting`/`tracking` following the ray-hit count.

`acquisition` and `preprocessing` stay flat and comparable to HotSpot precisely
because they never touch the volume — which is what rules out a general
interpreter/dispatch regression and points at the segment path specifically.

## Fixed here: the code-buffer estimate ignored nested inline bodies

Independent of the above, and a real defect.

`x64::compile_with_param_slots` sizes two reservations from its inline plan —
the code buffer (`callee_code_len * 64`) and the spill reserve
(`max(callee_max_locals, param_span) + callee_code_len`). Both iterated the
TOP-LEVEL `inline_sites` only. But `InlineSite::nested_sites` holds full
recursive `InlineSite`s and the emitter splices those bodies into the SAME
buffer, so every site that inlines anything itself was under-counted. The
per-method inline BUDGET was already nested-aware
(`inline_site_expansion_cost_tiered` folds `nested_expansion`); only the two
sizing sites were not.

Measured on this app:

```
JIT compile bailed: code buffer estimate too small; retrying at the measured size
  method=VolumeOps.grad(...)  code_len=1511 capacity=433312 wanted=468437   (8% short)
  method=IterativeClosestPoint.reduce(...) code_len=846 capacity=200000 wanted=211713
```

Both are methods whose callees are all tiny getters that each splice a
constructor and three field loads — exactly the shape the missing nested term
under-counts. With the fix `VolumeOps.grad` no longer overruns its estimate at
all.

Scope of the win, stated honestly: the retry path already recovered these (each
method bailed ONCE, then compiled at the doubled size), so this buys back one
wasted full lowering per affected method and makes the estimate honest — it is
**not** the fix for the 126x. `IterativeClosestPoint.reduce` still overruns by
5.8%; its estimate is exactly `code_len*96 + 8192 + invokes*1024` with no inline
sites at all, so that is a separate thinness in the base heuristic (most likely
loop unrolling / pc replication, which the estimate does not model) and the
retry still covers it.

Regression test: `s31_inline_reservations_count_nested_bodies` — two arms, a
leaf site pinned to the exact pre-existing per-site formula (no-regression) and
a two-level nested tree, so a walk that stops one level early fails it.

## The OOM at frame 13

The original report's `memory allocation of 536870912 bytes failed` is NOT the
code-buffer retry: that path is capped at `MAX_CODE_BUFFER_RETRIES = 3`
doublings from a ~433 KB estimate, which cannot reach 512 MB. It is also not a
per-frame leak in the segment path — `heap_segment_view` is never even reached
for these segments (`Arena.ofAuto().allocate` mints
`cratonvm.internal.foreign.MemorySegmentImpl` with `isNative=true`).

**It is not a leak either.** A run at `-Xms1G` — an eighth of the original
`-Xms4G`, to separate "grows without bound" from "needs more than the machine
has" — settles at the same footprint and then stays there, including past frame
3 where raycasting (and with it `VolumeOps.grad`'s allocation storm) starts:

| frame | RSS | total |
|---|---|---|
| 0 | 3,585,636 KB | 118.7 s |
| 1 | 3,585,640 KB | 98.9 s |
| 2 | 3,587,192 KB | 97.9 s |
| 3 | 3,588,604 KB | 230.7 s |
| 4 | 3,591,896 KB | — |

~6 MB of drift across four frames, with the per-frame time meanwhile growing
2.3x. A leak that tracked the work would not look like this, and `-Xms1G` and
`-Xms4G` converging on ~3.6 GB says the number is the workload's steady-state
footprint, not the initial heap.

So the original abort is best explained as the box running out of RAM: a 4 GB
pre-allocated heap plus a ~3.6 GB working set, on a shared host that was also
running another session's `rustc` (2 processes, ~4 GB resident) — the same
contention that made every wall-clock arm on this page need an in-run control.
It did not reproduce here in 5 frames at `-Xms1G`.

What IS worth pursuing is the ~3.6 GB itself, which is far more than the
workload needs (the 256³ TSDF volume is 67 MB, and HotSpot runs the whole
pipeline in well under 1 GB). The likely driver is allocation RATE rather than
retention: `VolumeShort2.get` mints a `Short2` per voxel read and `VolumeOps.grad`
allocates dozens of `Float3`/`Int3` per call, so integration alone churns ~16.7M
short-lived objects per frame. That is a separate, unmeasured question — filed
here rather than answered.

## The fix this needs

Intrinsify FFM element access in the JIT so the call disappears. Everything
else is arithmetic around a ~300 ns floor that only removing the call can beat.

The machinery exists and there is a close template — the `AtomicInteger` RMW
intrinsic in `x64/bytecode_walk.rs` (`INTRINSIC REGION: ATOMIC_INT`): resolve in
`try_resolve_intrinsic` by `(class, name, descriptor)`, then emit inline with a
null check and an exact receiver class-id guard, sending every uncertain case to
the shared uncommon-trap stub.

What makes it tractable: **the call site's descriptor names the layout type**
(`getAtIndex:(Ljava/lang/foreign/ValueLayout$OfShort;J)S`), so the element width
and result kind are known at compile time and no constant-propagation of the
`getstatic ValueLayout.JAVA_SHORT` operand is required.

What makes it dangerous, and what a correct implementation must guard — each of
these is a memory-safety obligation the native currently discharges per call:

* **arena liveness.** `pe_segment_check_scope` refuses a closed arena. An
  intrinsic that skips it turns use-after-close into a raw read of freed memory.
* **heap-backed segments.** `MemorySegment.ofArray(...)` has no address to load
  from; those must bail to the call.
* **bounds.** `0 <= offset && offset + width <= size`, overflow-checked, raising
  `IndexOutOfBoundsException` (not `IllegalStateException` — see the measured
  oracle in `pe_segment_access_addr`).
* **read-only** for the `set` side.
* the per-object COMPACT/LEGACY field-layout branch, as the `AtomicInteger`
  intrinsic does.

Ship it behind a kill switch and price it on this app: `integration` at 57 s/frame
is ~16.7M voxels × 2 accesses, so a correct intrinsic should take the frame floor
from ~80 s to a few seconds.

## Repro

```bash
cd apps/kfusion-tornadovm
CP="target/classes:target/*.jar"     # ';' separated on Windows
java        -Xms4G -cp "$CP" kfusion.java.Benchmark conf/bm-traj2-local.settings
cratonvm.exe --java-home <jdk25> -Xms4G -cp "$CP" kfusion.java.Benchmark conf/bm-traj2-local.settings
```

The per-element microbenchmarks are the faster loop for anyone working on the
intrinsic — they reproduce the whole finding in ~30 s without the dataset.

## Getting the dataset (`scene2raw` does not exist upstream)

`downloadDataSets.sh` wants `slambench/build/kfusion/thirdparty/scene2raw`,
which exists in no reachable form: the current `pamela-project/slambench` was
restructured into SLAMBench2 (kfusion is now a separately-cloned `.so` module
with no standalone dataset tooling), `pamela-project/kfusion` does not have it,
`GerhardR/kfusion` is a live-camera CUDA demo, and no tag or branch preserves
the pre-2017 layout the script was written against. Building the SLAMBench C++
framework to check was a multi-hour GCC15/CMake4 exercise and produced no
converter.

Replacement: `scene2raw.py` (numpy + Pillow) reads the ICL-NUIM
`living_room_traj2_loop.tgz` members directly, converts the ASCII `.depth`
Euclidean ray length to perpendicular Z with the app's own intrinsics
(`fx=481.2, fy=480, cx=320, cy=240`, from `RawDevice.CAMERA`),

```
Z = L / sqrt(1 + ((u-cx)/fx)^2 + ((v-cy)/fy)^2)
```

and writes millimetre `uint16`s in the layout reverse-engineered from
`RawDevice.java`: per frame an 8-byte ignored header, `w*h` depth `uint16` LE,
an 8-byte ignored header, `w*h` RGB byte triples — matching
`calcFrameSize() = 16 + w*h*5`. First 50 of 882 frames converted. Validated by
running the real CPU pipeline on HotSpot against it: ICP produced small,
monotonically evolving pose deltas, so the depth conversion is sane and not
structurally-well-formed noise.

`conf/bm-traj2-local.settings` points `kfusion.raw.file` at the converted file's
absolute path, which bypasses `RawDevice`'s `http:` auto-download branch.

## Related files

- `apps/kfusion-tornadovm/src/main/java/kfusion/java/algorithms/Raycast.java`
- `apps/kfusion-tornadovm/src/main/java/kfusion/java/algorithms/Integration.java`
- `native-builtins/src/panama.rs` — `pe_segment_get_at_index`, `pe_segment_access_addr`
- `native-builtins/src/panama_libffi.rs` — `segment_address`, `segment_byte_size`
- `jit/src/x64/driver.rs` — `spliced_bytecode_len` / `spliced_stack_reserve` (fixed here)
- `jit/src/x64/bytecode_walk.rs` — `INTRINSIC REGION: ATOMIC_INT`, the template
