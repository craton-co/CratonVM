# CratonVM GPU build — `native-builtins` rustc crash (exit `0xffffffff`)

## Status
**OPEN** (2026-06-05). CPU build succeeds; GPU feature build dies mid-compile.

## Severity
**HIGH** — no `target-gpu/release/cratonvm.exe`; all `--gpu` benchmarks and
`gpu-offload` suites unavailable.

## Symptom

```bash
cargo build --release -p cratonvm-cli --bin cratonvm \
  --features gpu-driver --target-dir target-gpu -j 1
```

Build progresses through `cratonvm-gpu`, `cratonvm-jit-cuda`, `cratonvm-vm`, then:

```
Compiling cratonvm-native-builtins v0.3.0 (C:\craton\CratonVM\native-builtins)
```

Process terminates with **exit code `0xffffffff` (-1)**. No `Finished release` line.
No Rust error message in the log — the compiler process is killed (typically
**stack overflow in `rustc`** while type-checking the ~15k-line `native-builtins` crate).

**Logs:**
- `build_gpu_isolated.log` — stops at `native-builtins` warnings
- `build_gpu_isolated2.log` — same

## What succeeds

CPU build with isolated target dir (same machine, same day):

```bat
set RUST_MIN_STACK=536870912
cargo build --release -p cratonvm-cli --bin cratonvm --target-dir target-fresh-cpu -j 1
```

→ `Finished release` in **12m 56s** → `target-fresh-cpu/release/cratonvm.exe` (18 MB).

So the failure is **not** a missing toolchain — it is specific to the GPU
feature closure (`gpu-driver` → `cratonvm-gpu`, `cratonvm-jit-cuda`, extra deps)
or concurrent `target/` lock contention when sharing the default target dir.

## Downstream impact

| Workload | Without GPU binary |
|----------|-------------------|
| `bench-suite-4way` `cratonvm-gpu` column | rc=127, CRASH (`No such file or directory`) |
| `apps/_test-suites/gpu-offload/GpuProbe` | Cannot run |
| `apps/gpu-bench/` GPU paths | Cannot run |
| Micro-benchmarks `--gpu` | N/A (CPU-only numbers only) |

GPU micro-benchmarks would match CPU anyway for non-offload-eligible kernels
(`arith1500M`, `fib44`, etc.) — but **GpuCompute / GpuProbe** need the binary.

## Known mitigations (from `test_prompt.md`)

1. **Kill stray `cratonvm.exe`** before build (`taskkill /F /IM cratonvm.exe`) —
   a running VM locks the output exe for relink.
2. **`RUST_MIN_STACK=536870912`** — raises rustc thread stack (helps CPU build;
   GPU build still failed in 2026-06-05 retries).
3. **Isolated `--target-dir`** — avoid cargo lock / stale rlib races:
   - CPU: `target-fresh-cpu` (works)
   - GPU: `target-fresh-gpu` (failed twice at `native-builtins`)
4. **`-j 1`** — reduce parallel memory pressure during `native-builtins`.
5. If exit `0xffffffff` persists: retry after closing other `cargo` agents, or
   build GPU on a machine with more RAM / swap.

## Repro (Windows, Git-bash or cmd)

```bat
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"
set RUST_MIN_STACK=536870912
cargo build --release -p cratonvm-cli --bin cratonvm --features gpu-driver --target-dir target-fresh-gpu -j 1
```

Verify success:

```bat
dir target-fresh-gpu\release\cratonvm.exe
```

Mtime must advance after each build attempt.

## Fix direction

1. **Reduce `native-builtins` compile-unit depth** — split modules or `#[inline(never)]`
   on worst offenders so GPU+CPU feature unification does not blow rustc stack.
2. **CI split:** build CPU and GPU in separate jobs with separate `target-dir`.
3. **Document minimum RAM** for GPU link (GPU closure pulls `cudarc`, `cratonvm-jit-cuda`).

## Related files

- `build-gpu.bat`, `build-gpu-isolated.bat`
- `apps/_test-suites/gpu-offload/README.md`
- Bench harness: `test-infra/bench-suite-4way.sh` (`cratonvm-gpu` variant)
- Comparison handoff: `docs/comparison-handoff/bug-gpu-offload-launch-glue-stub.md`
