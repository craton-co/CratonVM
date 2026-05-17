# GPU offload for CratonVM — reference documentation

This directory is the single source of truth for CratonVM's GPU
offload feature. **The feature is opt-in.** A default build of the
JVM is byte-identical to the pre-GPU codebase: no `cuda-bridge` link,
no `--gpu*` CLI flags, no extra branches in the interpreter hot path.

If you only want to run the JVM on CPU, you can stop reading. If you
want to enable GPU offload, route a recurring Java workload through
it, or extend the feature, read on.

## Document map

| File | What it covers |
| --- | --- |
| **`README.md` (this file)** | Top-level reference: what exists, how it fits together, how to build / run / test, file index. |
| [`plan.md`](plan.md) | The original execution plan with per-part status, deviations from spec, and known follow-ups. |
| [`cuda-oxide-evaluation.md`](cuda-oxide-evaluation.md) | Why [cuda-oxide](https://nvlabs.github.io/cuda-oxide/) (the project that prompted this work) is not on the critical path, and the precise conditions under which we would re-evaluate. |
| [`first-results.md`](first-results.md) | Acceptance-criteria scaffold for the GPU-equipped verification machine. Empty results table until real numbers land. |
| [`annotations.md`](annotations.md) | Phase 1 reference: `@GpuKernel`, `@GpuExclude`, `@EnableGpuAsync` directives for user-facing offload control. |
| [`streams-events.md`](streams-events.md) | Phase 2 reference: `Stream`, `Event`, async memcpy, `launch_on_stream`, stub op log for tests. |
| [`async-api.md`](async-api.md) | Phase 3 reference: `GpuExecutor`, `GpuFuture<T>`, `GpuArray<T>`, `GpuStream` for explicit async offload. |

## At a glance

GPU offload identifies *pure static methods over primitive arrays*,
lowers them from Java bytecode to NVIDIA PTX, and executes them on a
CUDA device. Anything that doesn't match the supported shape — calls,
allocation, field access, monitor ops, exceptions, reference arrays —
falls through to the existing interpreter / JIT without observable
difference.

The first offload target is methods like:

```java
public static void vectorAdd(int[] a, int[] b, int[] out) {
    int n = a.length;
    for (int i = 0; i < n; i++) {
        out[i] = a[i] + b[i];
    }
}
```

Real fixtures live under [`test_classes/gpu/`](../../test_classes/gpu/).

## Build modes

GPU offload is gated behind Cargo features. **Three valid build
levels:**

| Invocation | What you get |
| --- | --- |
| `cargo build` | CPU-only JVM. No GPU code linked. No `--gpu*` flags visible. **CPU build is unchanged.** |
| `cargo build --features gpu` | Above, plus the `cuda-bridge` crate in **stub mode** (probes return `DeviceError::NoDriver`) and the `--gpu*` CLI flags. Useful for testing the GPU plumbing on machines without a driver. |
| `cargo build --features gpu-driver` | Above, plus real `cudarc` bindings to `libcuda` / `nvcuda.dll`. Requires CUDA Toolkit 12.x. |

The features compose: `gpu-driver` implies `gpu`, which (when applied
to `rustjvm-cli`) propagates `rustjvm-vm/gpu-offload`, which in turn
propagates `rustjvm-gc/gpu-offload`. Pulling on the CLI's `gpu`
feature drags the entire stack in; pulling on nothing leaves the CPU
path pristine.

## CLI surface (only under `--features gpu`)

| Flag | Default | Meaning |
| --- | --- | --- |
| `--gpu` | off | Enable GPU offload of eligible static methods. |
| `--gpu-device <N>` | 0 | CUDA device ordinal. |
| `--gpu-min-work <N>` | 4096 | Skip offload when the method's estimated work is below this threshold (avoids round-trip overhead on tiny inputs). |
| `--print-gpu-decisions` | off | Log one `tracing::info!` line per analyzer verdict. |
| `--gpu-info` | n/a | Probe the device, print name + compute capability + memory, exit. No JVM bootstrap. |

When `--gpu` is requested but the driver is missing:

```
[rustjvm-cli] --gpu requested but no CUDA driver available (no CUDA driver available …); running on CPU
```

The flag is silently demoted; the program continues on CPU.

## Architecture

```
                    ┌──────────────────────────────┐
                    │     vm-cli (--features gpu)   │
                    │  parses --gpu*, plumbs into   │
                    │  VmConfig.gpu_offload_enabled │
                    └────────────────┬─────────────┘
                                     │
                ┌────────────────────▼─────────────────────┐
                │   rustjvm-vm (cfg gpu-offload)            │
                │                                          │
                │  execute_invokestatic() hook ─────────┐  │
                │       │                                │ │
                │       ▼                                │ │
                │  runtime::offload::try_dispatch ────┐  │ │
                │       │                              │ │ │
                │       ▼                              │ │ │
                │  OffloadCache.lookup_or_compile     │ │ │
                │       │  (analyze → lower → load)   │ │ │
                │       ▼                              │ │ │
                │  CompiledKernel ◄─── jit-cuda ◄─────┘ │ │
                │       │                                │ │
                │       │   ┌─── gpu_marshal ────┐       │ │
                │       │   │  host_view_<T> /   │       │ │
                │       │   │  write_back_<T>    │       │ │
                │       │   └────────────────────┘       │ │
                │       ▼                                │ │
                │  cuda-bridge ──── DeviceContext        │ │
                │                   DeviceModule         │ │
                │                   DeviceBuffer<T>      │ │
                │                   launch_raw           │ │
                └────────────────────┬───────────────────┘
                                     │
                ┌────────────────────▼─────────────────────┐
                │   rustjvm-gc (cfg gpu-offload)            │
                │   SafepointToken + Heap::pin_ref          │
                │   GC delays while any token is alive      │
                └──────────────────────────────────────────┘
```

## The seven new pieces

### 1. `cuda-bridge` (new crate)

[`cuda-bridge/`](../../cuda-bridge/)

Thin, JVM-agnostic CUDA Driver API wrapper. **Two backends:**

- `backend_stub.rs` — default. Every fallible call returns
  `DeviceError::NoDriver`. Enables the workspace to build on machines
  without a CUDA toolkit.
- `backend_cuda.rs` — gated by the `cuda` feature. Real bindings via
  the [`cudarc`](https://crates.io/crates/cudarc) crate (pinned to
  `cuda-12060`).

Public surface:

| Type / fn | Purpose |
| --- | --- |
| `probe() -> Result<DeviceCaps>` | Discover the attached GPU. |
| `DeviceContext::new(ordinal: u32)` | Acquire / attach to a context. |
| `DeviceModule::from_ptx(&ctx, ptx, &[kernel_names])` | Load a PTX text module and resolve named kernels. |
| `DeviceBuffer<T>` | Typed device-side allocation: `uninit`, `zeros`, `from_host`, `to_host`. `T: bytemuck::Pod`. |
| `KernelArgs` | Builder pattern: `push_device_ptr`, `push_i32`/`i64`/`f32`/`f64`. |
| `LaunchConfig` + `LaunchConfig::elementwise(n)` | Grid/block dimensions. |
| `module.launch_raw(&ctx, name, &cfg, args)` | Fire-and-forget kernel launch. Pair with `ctx.synchronize()`. |
| `DeviceError` | `NoDriver` + per-stage variants (`Driver`, `Load`, `KernelNotFound`, `Launch`, `Memcpy`). |

### 2. `jit-cuda` (new crate)

[`jit-cuda/`](../../jit-cuda/) — Java bytecode → PTX lowering.

Two stages:

- **`analyzer`** (`src/analyzer.rs`): `analyze(method) -> OffloadVerdict`. Static gate that decides whether a method is GPU-eligible. Rejects: non-static, synchronized, native/abstract, non-primitive params, allocation, calls, fields, type checks, monitors, switches, throws, jsr/ret, reference arrays. Estimates work via a backward-branch count.
- **`lowering`** (`src/lowering.rs` + `loop_recog.rs` + `emit.rs`): consumes the bytecode of an eligible method and produces a `PtxModule`. Two-stage design:
  - `loop_recog` recognises the canonical counted-loop pattern. Returns `UnsupportedNode` for anything fancier (multiple back-edges, nested loops).
  - `emit` walks the bytecode, simulates the JVM operand stack with PTX virtual registers, emits PTX text.

The emitted kernel is SIMT element-wise:

1. Compute `tid = ctaid.x * ntid.x + tid.x`.
2. Bounds-check `tid` against the loop bound; if outside, `ret`.
3. Emit the loop body with `iload <iv>` rewritten to `tid` and `iinc <iv>` skipped.
4. Every `*aload` / `*astore` carries a bounds check that jumps to a shared `L_bounds_fail` label which writes `1` to `*failure_flag` (a u32 device buffer) and `ret`s.

**Kernel parameter convention** (final; matches `build_param_list` in `lowering.rs` and the marshalling layout in `gpu_marshal.rs`):

| Java parameter | PTX slots |
| --- | --- |
| `int` / `long` / `float` / `double` | one `p<i>: .s32/.s64/.f32/.f64` |
| Primitive array | two slots: `p<i>_ptr: .u64`, `p<i>_len: .s32` |
| Array return | trailing `ret_ptr: .u64`, `ret_len: .s32` |
| Scalar return | trailing `ret_ptr: .u64` (one-element output buffer) |
| Always last | `failure_flag: .u64` |

### 3. `vm::runtime::gpu_marshal` (`#[cfg(feature = "gpu-offload")]`)

[`vm/src/runtime/gpu_marshal.rs`](../../vm/src/runtime/gpu_marshal.rs) — JVM heap ↔ device memory.

For each primitive type (`i32`, `i64`, `f32`, `f64`, `i16`, `i8`):

- `host_view_<T>(obj, &heap, &SafepointToken) -> Vec<T>` — pack a Java primitive array into a host buffer for upload.
- `write_back_<T>(obj, &mut heap, src, &SafepointToken)` — copy a packed host buffer back into a JVM array's storage.

Reuses existing `Heap::get_array_element` / `set_array_element` so it stays correct regardless of the heap's native stride.

Generic helpers `upload<T>(&ctx, &[T]) -> Result<DeviceBuffer<T>>` and `download_into<T>(&buf, &mut [T])` wrap `cuda-bridge`. The `&SafepointToken` parameter is a type-system marker: holding it proves we are in a no-GC critical section (see Part F below).

### 4. `vm::runtime::offload` (`#[cfg(feature = "gpu-offload")]`)

[`vm/src/runtime/offload.rs`](../../vm/src/runtime/offload.rs) — the cache + dispatch hook.

| Item | Purpose |
| --- | --- |
| `CompiledKernel` | Loaded PTX `DeviceModule` + the `KernelSignature` + the mangled kernel name. |
| `OffloadCache` | Per-VM cache. Lazily probes the device. `lookup_or_compile(class_id, class_name, method_index, &method) -> LookupOutcome`. |
| `LookupOutcome::{Hit, Skip, Blacklisted}` | Cache decisions. `Hit` carries an `Arc<CompiledKernel>`. `Skip` is silent fall-through (no device, or method work is below `--gpu-min-work`). `Blacklisted` is "we already rejected this method; don't re-analyze". |
| `DispatchOutcome::{Handled, FallThrough}` | What the interpreter sees from `try_dispatch`. Today every path returns `FallThrough`; see "Known follow-up" below. |
| `try_dispatch(shared, thread, frame_idx, class, method, descriptor, args)` | Entry point called from the interpreter hook. Signature is **final**. |
| `output_array_index(&param_kinds)` | First-cut convention: the *last* array parameter is the output sink. Matches `EligibleVectorAdd(a, b, out)`. |

### 5. Interpreter integration (`vm/src/runtime/interpreter.rs`)

The hook lives in `execute_invokestatic`, immediately before the `try_stackless_invoke` call (around line 9709). Behind `#[cfg(feature = "gpu-offload")]`, double-guarded by `shared.config.gpu_offload_enabled && shared.offload_cache.has_device()`. On `DispatchOutcome::Handled` the function returns `CachedCallResult::Handled` (with the operand stack already prepared). On `FallThrough` the existing CPU path runs unchanged.

**The default build's `execute_invokestatic` has zero new branches.** With `gpu-offload` off, the cfg-gated block is removed by the preprocessor.

### 6. `gc::safepoint` (`#[cfg(feature = "gpu-offload")]`)

[`gc/src/safepoint.rs`](../../gc/src/safepoint.rs) + additions to [`gc/src/heap.rs`](../../gc/src/heap.rs).

| Item | Purpose |
| --- | --- |
| `SafepointToken<'h>` | Zero-sized, `!Send`, RAII. Drop decrements an atomic on the heap. Construction is gated through `Heap::enter_gpu_critical()`. |
| `Heap::enter_gpu_critical() -> SafepointToken<'_>` | Acquire a token. Increments `gpu_critical_count`. |
| `Heap::pin_ref(obj)` / `unpin_ref(obj)` | Mark/unmark a JVM `ObjectRef` as a GC root for the duration of a kernel. |
| GC delay inside `collect_garbage` / `collect_garbage_with_finalizers` | While `gpu_critical_count > 0` the collector yield-spins (warns after 5s; **never** collects mid-token). |
| `gpu_critical_count()` / `gpu_blocked_gc_count()` | Observability accessors. |

The root walker visits pinned refs during collection so the JVM cannot move an array while a kernel reads it.

### 7. `jit-api::gpu_lowering` (`#[cfg(feature = "gpu-lowering")]`)

[`jit-api/src/gpu_lowering.rs`](../../jit-api/src/gpu_lowering.rs) — a single trait, `GpuLowering`, defining the contract any future PTX producer must satisfy. The only implementor today is `jit-cuda`'s `PtxEmitter`. The trait exists so that **if** a future story wants to add Rust-authored device helpers (parallel GC mark, atomic intrinsics) compiled via cuda-oxide and linked alongside our own emitter, it can plug in without touching `try_dispatch`. See [`cuda-oxide-evaluation.md`](cuda-oxide-evaluation.md) for the reasoning.

## How a method actually offloads (annotated walk)

Imagine `EligibleVectorAdd.vectorAdd(int[] a, int[] b, int[] out)` is invoked with `n = 1<<20`. With `cargo build --features gpu-driver` and `--gpu` on a real GPU:

1. The interpreter is about to dispatch `invokestatic vectorAdd`. The cfg-gated hook fires.
2. `try_dispatch` resolves the class via `shared.class_manager`, finds the method index, calls `OffloadCache::lookup_or_compile`.
3. First call: cache miss. The analyzer says `Eligible(KernelSignature{ param_kinds: [I32Array, I32Array, I32Array], return_kind: Void, estimated_work: 1<<20 })`.
4. `jit_cuda::lower_method` produces a PTX module. `loop_recog` accepts the counted loop; `emit` produces ~30 lines of PTX including `mad.wide`, `setp.ge.s32`, three `ld.global.s32`, one `add.s32`, one `st.global.s32`, the `L_bounds_fail` label.
5. `DeviceModule::from_ptx` loads the PTX onto the GPU. The `Arc<CompiledKernel>` is cached under `(ClassId, method_index)`.
6. **Launch glue (currently a follow-up — see below):**
   - Acquire `Heap::enter_gpu_critical()`.
   - For each `int[]` argument: `host_view_i32(obj, &heap, &token)` → `upload(&ctx, &host)` → `KernelArgs::push_device_ptr(&buf).push_i32(len)`. Pin every `ObjectRef`.
   - Allocate `DeviceBuffer::<u32>::zeros(&ctx, 1)` for `failure_flag`.
   - `module.launch_raw(&ctx, name, &LaunchConfig::elementwise(n), args)`, then `ctx.synchronize()`.
   - Read `failure_flag` back. If non-zero → unpin / drop token / return `FallThrough` (interpreter runs unchanged).
   - Otherwise `download_into` into a `Vec<i32>` and `write_back_i32` into the `out` array via the output-array-index convention. Unpin / drop token / push the return value (here: nothing — void return) → `DispatchOutcome::Handled`.
7. Subsequent calls skip the analyze/lower step (cache hit) and go straight to step 6.

## Testing

| Crate / target | Tests | What they prove |
| --- | --- | --- |
| `cargo test -p cuda-bridge` | 3 | `NoDriver` contract holds when the `cuda` feature is off. |
| `cargo test -p jit-cuda` | 23 (+2 `#[ignore]`) | Real `.class` fixtures lower to non-trivial PTX; all `Reject*` fixtures classify correctly; non-canonical control flow returns `Unsupported`. The 2 `#[ignore]` tests require `ptxas` and an attached GPU. |
| `cargo test -p rustjvm-jit-api` | 24 | Unchanged from before GPU work; `GpuLowering` trait compiles under `--features gpu-lowering`. |
| `cargo test -p rustjvm-gc --features gpu-offload` | 678 (= 672 pre-existing + 6 new) | Token increment/decrement, nested tokens, GC blocks while a token is held, pinned refs survive a real GC cycle (uses `Heap::alloc_array`), root walker visits pinned refs. |
| `cargo test -p rustjvm-vm --features gpu-offload --lib gpu_marshal` | 9 | Round-trip every primitive array type through a real `Heap`. |
| `cargo test -p rustjvm-vm --features gpu-offload --lib offload` | 6 | Cache skips on no-device, classifies eligible / ineligible methods from real `.class` files, disabled-config returns `Skip`. |

**All tests run on machines without an NVIDIA GPU.** Real-kernel verification on GPU hardware happens against `Benchmark.java` per [`first-results.md`](first-results.md).

### No synthetic stubs

Every test fixture is a real Java source compiled by `javac` via [`jit-cuda/build.rs`](../../jit-cuda/build.rs). There are zero hand-rolled bytecode arrays. The 15 fixtures in [`test_classes/gpu/`](../../test_classes/gpu/) cover:

- 3 eligible methods (`EligibleVectorAdd`, `EligibleSaxpy`, `EligibleDotProduct`)
- 7 rejected methods (`RejectAllocation`, `RejectInvoke`, `RejectSynchronized`, `RejectRefArray`, `RejectSwitch`, `RejectThrow`, `RejectFieldAccess`, `RejectTypeCheck` — covering 8 of the analyzer's `Reason` variants)
- 1 non-canonical control-flow shape (`TwoLoops` — exercises lowering's `Unsupported` path)
- 1 multi-threaded allocation stress program (`GcStress` — for Part F)
- 1 driver (`Benchmark`) for the eventual CPU vs GPU comparison

## Known follow-ups (all flagged in source with "GPU-required follow-up")

### The single critical follow-up: launch glue inside `try_dispatch`

[`vm/src/runtime/offload.rs`](../../vm/src/runtime/offload.rs) — the `LookupOutcome::Hit` arm currently logs `tracing::debug!` and returns `DispatchOutcome::FallThrough`. The signature, call-site contract, and `OffloadCache` lookup path are all final. Only the *body* of the `Hit` arm grows in the next iteration. The reason it's deferred: validating `cuLaunchKernel` + the deopt-on-failure dance requires actual NVIDIA hardware, and landing the surrounding cfg-gated wiring first lets us add the dispatch code in a single tightly-scoped change.

Steps to finish on a GPU box: see the "How a method actually offloads" walk above, steps 6.a through 6.f.

### Lower priority

- **`jit-cuda` opcode coverage:** `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` return `Unsupported` (rare in element-wise loops). `dup2_x1` / `dup2_x2` also `Unsupported`. `frem` / `drem` emit text that won't `ptxas`-clean (no native rem on f32/f64) — the analyzer admits them but the kernel will fail validation; a software-correct sequence would close this.
- **Reductions:** `EligibleDotProduct` lowers, but every CUDA thread races on `ret_ptr`. Result is only correct if every thread happens to compute the same value. A proper reduction kernel is needed for genuine sum-reductions.
- **Analyzer fixture gaps:** `Reason::Monitor` and `Reason::JsrRet` are unreachable through `javac` output (synchronized always emits an exception table, hitting `HasExceptionHandlers` first; `jsr`/`ret` were dropped from `javac` decades ago). These are documented gaps with no synthetic-stub workaround.
- **`Benchmark.java` real numbers:** awaiting a GPU box. The `docs/gpu/first-results.md` table is empty until then.
- **GC stress integration test:** `GcStress.java` exists; a Rust integration test under `vm/tests/` that actually runs it while another thread holds a `SafepointToken` belongs to the next iteration.

## File index (just the GPU-touching files)

```
cuda-bridge/                         New crate. Optional dep of vm-cli.
  Cargo.toml, README.md
  src/
    lib.rs                           Public API + stub-mode tests
    backend_cuda.rs                  cudarc backend (feature = "cuda")
    backend_stub.rs                  No-driver backend (default)

jit-cuda/                            New crate. Used by rustjvm-vm under gpu-offload.
  Cargo.toml, build.rs               build.rs compiles test_classes/gpu/*.java
  src/
    lib.rs
    analyzer.rs                      OffloadVerdict, ParamKind, Reason
    emitter.rs                       PtxModule / PtxKernel / RegKind
    signature.rs                     KernelSignature
    lowering.rs                      lower_method entry point
    lowering/loop_recog.rs           Canonical counted-loop recogniser
    lowering/emit.rs                 Opcode walker + register allocator
    test_support.rs                  load_method(class, method, descriptor)

jit-api/
  Cargo.toml                         Added gpu-lowering feature
  src/lib.rs                         Conditionally exposes gpu_lowering module
  src/gpu_lowering.rs                GpuLowering trait + LoweredKernel

gc/
  Cargo.toml                         Added gpu-offload feature
  src/lib.rs                         Conditionally exposes safepoint module
  src/safepoint.rs                   SafepointToken
  src/heap.rs                        Gated fields + enter_gpu_critical + pin_ref + GC delay

vm/
  Cargo.toml                         Added gpu-offload feature, optional cuda-bridge / jit-cuda
  src/config.rs                      Gated VmConfig fields (gpu_offload_enabled, ...)
  src/runtime/mod.rs                 Gated module declarations
  src/runtime/gpu_marshal.rs         Heap ↔ device marshalling
  src/runtime/offload.rs             OffloadCache + try_dispatch
  src/runtime/interpreter.rs         Gated hook in execute_invokestatic
  src/vm/vm_init.rs                  Gated offload_cache field on SharedVm

vm-cli/
  Cargo.toml                         Added gpu / gpu-driver features, optional cuda-bridge
  src/main.rs                        Gated --gpu* flags + --gpu-info early-exit + plumbing

test_classes/gpu/
  README.md                          The "no synthetic stubs" rule
  *.java                             15 real Java fixtures (see Testing above)
  *.class                            javac output, checked in as intentional fixtures

docs/gpu/
  README.md                          This file
  plan.md                            Execution plan with per-part status
  cuda-oxide-evaluation.md           Why cuda-oxide is not on the critical path
  first-results.md                   Acceptance-criteria scaffold

Cargo.toml                           Added cuda-bridge and jit-cuda to workspace members
Cargo.lock                           Locked cudarc + bytemuck transitives
README.md                            Link to cuda-oxide-evaluation.md
```

## FAQ

**Q: Why feature flags instead of a runtime config flag?**
A: A runtime flag would leave dead branches in the interpreter hot path. The `#[cfg(feature = "gpu-offload")]` discipline means the compiled CPU binary contains *no* GPU code at all — easier to audit and impossible to regress accidentally.

**Q: Why not use cuda-oxide directly?**
A: See [`cuda-oxide-evaluation.md`](cuda-oxide-evaluation.md). Short version: cuda-oxide compiles *Rust source* to PTX. Our problem is *Java bytecode* to PTX. cuda-oxide doesn't help with that path. It might be useful much later for Rust-authored device helpers (parallel GC mark, atomic intrinsics) — the `GpuLowering` trait keeps that door open without a hard dependency today.

**Q: What happens to a Java exception thrown inside an offloaded method?**
A: Currently, only `ArrayIndexOutOfBoundsException`-equivalent failures are handled — the kernel writes `1` to `*failure_flag` and `ret`s; the host detects this and deopts to the interpreter, which observes no partial GPU state (input arrays are *not* written by the bounds-failed path; output arrays are independent buffers materialized on the host *after* the kernel succeeds). `ArithmeticException` for integer division by zero is a planned addition. Any other Java exception means the method isn't actually offload-eligible — the analyzer rejects it.

**Q: How do I add support for a new opcode?**
A: Three places.
1. Make sure `jit-cuda/src/analyzer.rs::classify()` admits the opcode (or doesn't reject it for the wrong reason).
2. Add the emission case in `jit-cuda/src/lowering/emit.rs`.
3. Add a real `.java` fixture under `test_classes/gpu/` that uses the opcode and a test in `jit-cuda/src/analyzer.rs` or a new lowering test that asserts the PTX content contains the right instruction.

**Q: How do I add a new GPU lowering backend (e.g. cuda-oxide-emitted helpers)?**
A: Implement the `jit_api::gpu_lowering::GpuLowering` trait. Wire it into `OffloadCache::new`'s lowering chain. The interpreter hook never has to change.

## Verification snapshot (no-GPU dev box)

```
$ cargo check --workspace                                 ✔ clean
$ cargo check --workspace --features rustjvm-cli/gpu      ✔ clean
$ cargo test -p cuda-bridge                               ✔ 3 passed
$ cargo test -p jit-cuda                                  ✔ 23 passed, 2 ignored
$ cargo test -p rustjvm-jit-api                           ✔ 24 passed
$ cargo test -p rustjvm-gc --features gpu-offload         ✔ 678 passed
$ cargo test -p rustjvm-vm --features gpu-offload \
      --lib gpu_marshal                                   ✔ 9 passed
$ cargo test -p rustjvm-vm --features gpu-offload \
      --lib offload                                       ✔ 6 passed
$ ./target/debug/rustjvm.exe --help | grep -i gpu         (no matches — clean)
$ ./target/debug/rustjvm.exe --gpu-info                   error: unexpected argument '--gpu-info' found
                                                          (correct — gpu feature is off)
```
