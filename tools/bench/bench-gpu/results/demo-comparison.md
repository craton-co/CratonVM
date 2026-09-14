# craton-gpu Demo — CratonVM (library) vs TornadoVM

**CratonVM-GPU:** `C:/craton/CratonVM/target-gpu/release/cratonvm.exe` (gpu-driver) via the craton-gpu library (`C:/craton/gpu-java/target/craton-gpu-0.2.0.jar`)
**TornadoVM:** `C:/craton/tornadovm/jdk-25.0.3/bin/java.exe` (PTX backend, RTX 2060)
**Kernel:** `heavy` — 96 integer multiply-adds per element (compute-bound), reps=3, best-of.

Both modes run the identical computation. "CratonVM-GPU" is offloaded
through `GpuExecutor.submit(class, method, descriptor, args)` (the
public craton-gpu API, real native bridge); "TornadoVM-GPU" uses
`@Parallel` + `TaskGraph`/`TornadoExecutionPlan`. "CratonVM-CPU(JIT)"
is the same kernel kept on the CPU via `@GpuExclude` (fair JIT baseline).

| N | CratonVM-CPU (JIT) | CratonVM-GPU (craton-gpu lib) | TornadoVM-GPU | GPU speedup vs CPU | checksum |
|---|---|---|---|---|---|
| 2^20 | 183ms | 1ms | 3ms | 183x | ✓ identical |
| 2^22 | 714ms | 2ms | 6ms | 357x | ✓ identical |
| 2^24 | 2946ms | 11ms | 21ms | 268x | ✓ identical |
| 2^26 | 10077ms | 38ms | 71ms | 265x | ✓ identical |

Checksums: CratonVM `-9127329792` … (identical to TornadoVM and the inline CPU spot-check).
