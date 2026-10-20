# GPU Offload Comparison — CratonVM vs TornadoVM

**Date:** 2026-06-16T22:30:30Z
**CratonVM:** `C:/craton/CratonVM/target/release/cratonvm.exe`
**CratonVM-GPU:** `C:/craton/CratonVM/target-gpu/release/cratonvm.exe`
**TornadoVM:** `C:/craton/tornadovm/jdk-25.0.3/bin/java.exe` (PTX/RTX 2060)
**HotSpot:** `C:/Program Files/Java/jdk-25/bin/java.exe`

## GpuCompute.heavy — 96× multiply-add per element (compute-bound)

| N | CratonVM CPU | CratonVM GPU | HotSpot CPU | TornadoVM GPU | CV-GPU speedup | TVM speedup |
|---|---|---|---|---|---|---|
| 2^20 (1.1M) | 158ms | 8ms | 23ms | 3ms | 2.9× | 7.7× | ✓ |
| 2^22 (4.2M) | 730ms | 16ms | 23ms | 6ms | 1.4× | 3.8× | ✓ |
| 2^24 (17M) | 2676ms | 45ms | 27ms | 20ms | 0.6× | 1.4× | ✓ |
| 2^26 (68M) | 10384ms | 130ms | 60ms | 83ms | 0.5× | 0.7× | ✓ |
| 2^28 (269M) | 40602ms | 655ms | 204ms | 323ms | 0.3× | 0.6× | ✓ |

## GpuProbe.vaddMap — out\[i\]=a\[i\]+b\[i\] (memory-bound, checksums)

| N | CratonVM (checksum) | HotSpot (checksum) | TornadoVM GPU |
|---|---|---|---|
| 2^20 (1.1M) | CS:550279050800 | CS:550279050800 | 4ms CS:550279050800 |
| 2^22 (4.2M) | CS:8798185971448 | CS:8798185971448 | 9ms CS:8798185971448 |
| 2^24 (17M) | CS:140745860167760 | CS:140745860167760 | 30ms CS:140745860167760 |
| 2^26 (68M) | CS:2251833301005528 | CS:2251833301005528 | 126ms CS:2251833301005528 |
| 2^28 (269M) | CS:- | CS:36028930968244920 | 455ms CS:36028930968244920 |

## Notes

- CratonVM GPU: automatic offload via `--gpu` flag (analyzes bytecode at first call)
- TornadoVM GPU: explicit `@Parallel` annotation + TaskGraph API (warmup call before measurement)
- All timings include H2D + kernel + D2H (full round-trip)
- TornadoVM heavy timing: warm (after PTX compilation in warmup call)
- CratonVM heavy timing: first call (includes PTX compilation + buffer alloc)
- N=2^28 included to verify if crash is fixed in current build
