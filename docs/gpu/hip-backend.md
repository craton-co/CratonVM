# HIP backend

The optional HIP backend uses pre-generated, dynamically loaded
`cubecl-hip-sys` bindings. Building it does not require a ROCm SDK. Running it
requires compatible AMD hardware and runtime libraries. Windows runtime loading
has a [known limitation](../known-issues/gpu/hip-windows-runtime-dll-name.md).

Kernel lowering renders HIP C++ from the common GPU representation in `jit-hip`.
Wavefront width is queried from the device: assumptions about 32-lane CUDA warps
must not be applied to 64-lane AMD wavefronts. Floating-point operations preserve
the supported Java contract and checksums determine semantic acceptance.

## Backend scope

HIP compiles generated source with `hiprtc` and loads the result through the
HIP runtime. It is a separate lowering and device-execution path; the NVIDIA
measurements in [BENCHMARK.md](../../BENCHMARK.md#gpu-benchmarks) do not measure
AMD throughput.

Graph capture/replay remains CUDA-specific in `cuda-bridge/src/graph.rs`;
non-CUDA builds return an unavailable-driver error for that surface. A
supported HIP kernel launch does not imply graph support. Unsupported kernel
instructions must be rejected by lowering so the caller can use the CPU path,
rather than receiving an approximate translation.

## Validation surfaces

Host-shim oracle tests compile generated C++ with a host compiler and exercise
both wavefront widths. Golden tests cover the renderer. Driver-absence tests
check graceful CPU fallback. Hardware integration tests check hiprtc compilation,
module loading, and launch; they require a real device and do not follow from
host-only tests. See `tools/gpu/amd-remote/README.md` for hardware commands.

Dynamic library loading can panic in the underlying bindings. Device probing
must contain that failure and return an unavailable backend so CPU fallback
remains usable. See [GPU architecture](README.md) and
[OpenCL](opencl-backend.md) for the other optional backends.

## Shared-Memory APUs (Ryzen AI)

Integrated AMD Ryzen AI APUs (`gfx1103`, `gfx1150`, `gfx1151`, `gfx1152`, `gfx1153`) share physical DRAM with the host CPU under wave32 execution. When `hipDeviceProp_tR0600` reports `integrated != 0` and `canMapHostMemory != 0`:

- `cuda-bridge` exposes unified host-mapped execution (`hipHostRegisterMapped` / `hipHostMallocMapped | hipHostMallocCoherent`) so slab pools spill transparently into system memory beyond BIOS UMA carve-out limits.
- Read-only Java primitive array arguments—and writable array arguments when the kernel has no bounds/deopt failure flag—are aliased zero-copy with `Relocation::Forbidden` and host-span pin caching instead of discrete H2D/D2H copies.
- Failure flags and scalar reduction accumulators allocate coherent host-mapped zeroed cells via `DeviceBuffer::new_shared_zeroed`.
- Built-in GEMM and fp16 conversion kernels (`vm/src/runtime/kernels/gemm.hip`) compile in-process via hiprtc without external header dependencies.
