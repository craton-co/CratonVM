# Optional cuda-oxide driver backend

The default CUDA bridge uses cudarc. `--features cuda-oxide`
`cratonvm-cuda-bridge`, or `--features gpu-driver-oxide` on `cratonvm-cli`,
selects the optional cuda-oxide driver backend.

The driver backend loads modules, manages streams and device memory, and
launches kernels. It does not replace CratonVM's Java-bytecode-to-GPU compiler.
Builds require the CUDA toolkit and binding-generation prerequisites; see the
crate's feature declarations and [GPU build guide](README.md).

Windows/MSVC builds have an upstream [enum-signedness limitation](../known-issues/gpu/cuda-core-msvc-enum-signedness.md).
Driver availability, module compilation, kernel checksums, and end-to-end Java
offload are separate checks. A successful host build does not establish device
execution or performance. Missing runtime support must fall back to CPU through
the bridge's existing error path.

## Driver comparison

Paired measurements on Windows 11, CUDA 13.3 and an RTX 2060 compare the
cuda-oxide driver with cudarc. Percentages are cuda-oxide time relative to
cudarc; positive values mean slower. A cudarc-versus-itself control measures
the noise floor.

| Workload | Rounds | cuda-oxide versus cudarc | Control |
|---|---:|---|---|
| `GpuTransferFloor`, 2^24 elements | 8 | +3.1% | +1.7%, 17% spread |
| `GpuAllocChurn`, 2^20 elements | 21 | +0.4%, 95% CI [-4.4%, +5.1%] | +1.8%, 95% CI [-1.9%, +5.5%] |

The allocation-churn comparison does not resolve a difference from zero at
this precision. Checksums match across driver backends. These measurements
do not cover graph capture or establish a throughput advantage for either
driver. Java kernel throughput comparisons belong to [BENCHMARK.md](../../BENCHMARK.md#gpu-benchmarks).
