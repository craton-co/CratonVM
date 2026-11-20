# GPU offload contract

GPU acceleration is opt-in. Only eligible pure static array kernels can be
lowered; unsupported shapes, missing drivers, and unsafe launch configurations
use CPU execution. Eligibility, launch work sizing, and minimum-work thresholds
are separate checks. A hardware checksum check must distinguish device work
from CPU fallback.

The compiler is in `jit-cuda`; HIP and OpenCL consume the common lowering
through their optional backends. The driver and memory bridge lives in
`cuda-bridge`, providing a stream-ordered caching `SubAllocator` backed by
2 MiB virtual memory slabs (`VmmPool` where supported, with driver allocation
fallback), `PinnedHostPool` staging and `HostRegionPinCache` heap-span
pinning for async DMA, `Relocation::Forbidden` and VRAM-pressure GC
coordination with `cratonvm-gc`, and device-resident `GpuFuture` array
chaining (`SerializedResult::DeviceArray` / `Native.futureThenApplyGpu`).
See [GPU reference](../gpu/README.md),
[launch sizing](../gpu/launch-work-sizing.md),
[streams and events](../gpu/streams-events.md),
[critical sections](../gpu/critical-sections.md), and
[JIT caller gate](../gpu/jit-caller-gate.md).
