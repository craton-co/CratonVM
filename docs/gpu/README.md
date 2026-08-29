# GPU offload for CratonVM — reference documentation

This directory is the single source of truth for CratonVM's GPU
offload feature. **The feature is opt-in.** A default build of the
JVM is byte-identical to the pre-GPU codebase: no `cuda-bridge` link,
no `--gpu*` CLI flags, no extra branches in the interpreter hot path.

If you only want to run the JVM on CPU, you can stop reading. If you
want to enable GPU offload, route a recurring Java workload through
it, or extend the feature, read on.

> **Status.** The full transparent-offload path — analyze
> → lower to PTX → load → marshal → launch → writeback/deopt — is
> implemented and validated on real hardware (RTX 2060, sm_75, CUDA
> driver 591.86). Checksums match HotSpot bit-for-bit on every kernel
> tested, including integer/long reductions and a four-sphere ray
> tracer whose per-pixel `a*b + c` chains are what first exposed the
> ptxas FMA-contraction hazard (see "Float bit-exactness" below). See
> [`../../README.md`](../../README.md)'s "GPU offload benchmarks"
> section and [`../book/src/gpu/benchmarks.md`](../book/src/gpu/benchmarks.md)
> for numbers. The hardware-validation follow-ups this status reflects are
> now complete; the durable technical documentation they produced is the
> feature-doc set in the table below.

## Document map

| File | What it covers |
| --- | --- |
| **`README.md` (this file)** | Top-level reference: what exists, how it fits together, how to build / run / test, file index. |
| [`COMPARISON.md`](COMPARISON.md) | CratonVM vs. TornadoVM / Project Babylon (HAT) / Aparapi / IBM SDK Java across 15 dimensions, with measured numbers. |
| [`cuda-oxide-evaluation.md`](cuda-oxide-evaluation.md) | Why [cuda-oxide](https://nvlabs.github.io/cuda-oxide/) (the project that prompted this work) is not on the critical path, and the precise conditions under which we would re-evaluate. |
| [`first-results.md`](first-results.md) | Pointer to where the real acceptance numbers ended up (`bench-gpu/results/`, the repo README) once a GPU box existed. |
| [`annotations.md`](annotations.md) | `@GpuKernel` (with `AdmissionHint`), `@GpuExclude`, `@EnableGpuAsync` directives for user-facing offload control, including the curated `Math`/`StrictMath` intrinsics table admitted under `ALLOW_INTRINSIC_CALLS`. |
| [`streams-events.md`](streams-events.md) | `Stream`, `Event`, async memcpy, `launch_on_stream`, per-buffer `last_write` ordering, stub op log for tests. |
| [`async-api.md`](async-api.md) | `GpuExecutor`, `GpuFuture<T>`, `GpuArray<T>`, `GpuStream` for explicit async offload — including the current completion model and stream-affinity behavior. |
| [`ci.md`](ci.md) | The self-hosted GPU CI workflow (`.github/workflows/gpu-selfhosted.yml`) and its checksum/latency gates (`bench-gpu/ci-gate.sh`). |
| [`reductions.md`](reductions.md) | Proven integer/long reductions, PTX reduction syntax, and the intentional floating-point exclusion. |
| [`jit-caller-gate.md`](jit-caller-gate.md) | Why eligible callers remain interpreted while GPU offload is active. |
| [`async-completion-reaper.md`](async-completion-reaper.md) | Callback-driven finalization and the completion reaper. |
| [`launch-work-sizing.md`](launch-work-sizing.md) | Runtime-sized grids for counted-loop kernels. |
| [`occupancy-launch-config.md`](occupancy-launch-config.md) | Occupancy-selected CUDA block configuration. |
| [`lowering-constants.md`](lowering-constants.md) | Numeric `ldc`/`ldc_w`/`ldc2_w` lowering. |
| [`lowering-fp-remainder.md`](lowering-fp-remainder.md) | Opt-in floating-point remainder lowering. |
| [`lowering-comparisons.md`](lowering-comparisons.md) | Bit-exact Java comparison opcodes. |
| [`lowering-offset-loops.md`](lowering-offset-loops.md) | Canonical counted loops with non-zero starts. |
| [`lowering-intrinsics.md`](lowering-intrinsics.md) | Curated `Math`/`StrictMath` intrinsic calls. |
| [`lowering-nested-loops.md`](lowering-nested-loops.md) | Rectangular two-dimensional loop mapping. |
| [`lowering-branches.md`](lowering-branches.md) | Basic-block and join-state lowering for loop-body branches. |
| [`hardware-ci.md`](hardware-ci.md) | Self-hosted GPU CI scaffolding and enrollment status. |
| [`../book/src/gpu/overview.md`](../book/src/gpu/overview.md) | User-facing book chapter: what can be offloaded, build modes, CLI flags. |
| [`../book/src/gpu/benchmarks.md`](../book/src/gpu/benchmarks.md) | The RTX 2060 benchmark writeup (methodology + tables). |

## At a glance

GPU offload identifies *pure static methods over primitive arrays*,
lowers them from Java bytecode to NVIDIA PTX, and executes them on a
CUDA device. Anything that doesn't match the supported shape falls
through to the existing interpreter / JIT without observable
difference.

The canonical offload target:

```java
public static void vectorAdd(int[] a, int[] b, int[] out) {
    int n = a.length;
    for (int i = 0; i < n; i++) {
        out[i] = a[i] + b[i];
    }
}
```

But the accepted shape is considerably broader today than that single
example suggests — see "What the analyzer accepts" below.

Real fixtures live under [`test_classes/gpu/`](../../test_classes/gpu/)
(32 real `.java` sources, no synthetic bytecode).

## Build modes

GPU offload is gated behind Cargo features. **Three valid build
levels:**

| Invocation | What you get |
| --- | --- |
| `cargo build` | CPU-only JVM. No GPU code linked. No `--gpu*` flags visible. **CPU build is unchanged.** |
| `cargo build --features gpu` | Above, plus the `cuda-bridge` crate in **stub mode** (probes return `DeviceError::NoDriver`) and the `--gpu*` CLI flags. Useful for testing the GPU plumbing on machines without a driver. |
| `cargo build --features gpu-driver` | Above, plus real `cudarc` bindings to `libcuda` / `nvcuda.dll`. Requires a CUDA driver at runtime (dlopened; no CUDA Toolkit install needed just to *run* a build, only to build one — `cudarc` links against the driver API headers at compile time). |

The features compose: `gpu-driver` implies `gpu`, which (when applied
to `cratonvm-cli`) propagates `cratonvm-vm/gpu-offload`, which in turn
propagates `cratonvm-gc/gpu-offload`, `cratonvm-native-builtins/gpu-offload`,
and pulls in `jit-cuda` + `cuda-bridge`. Pulling on the CLI's `gpu`
feature drags the entire stack in; pulling on nothing leaves the CPU
path pristine. `cratonvm-embed` mirrors the same `gpu` / `gpu-driver`
pair for embedders (the C-ABI `libcratonvm` does not expose GPU offload
at all — out of scope today).

Recommended build recipe (keeps the GPU build's artifacts out of the
CPU build's cache): `CARGO_TARGET_DIR=target-gpu cargo build --release
-p cratonvm-cli --bin cratonvm --features gpu-driver` — see
`build-gpu-driver.bat` at the repo root and
[`../../BUILD_GUIDE.md`](../../BUILD_GUIDE.md)'s "Building with GPU
offload" section.

## CLI surface (only under `--features gpu`)

| Flag | Default | Meaning |
| --- | --- | --- |
| `--gpu` | off | Enable GPU offload of eligible static methods. |
| `--gpu-device <N>` | 0 | CUDA device ordinal. |
| `--gpu-min-work <N>` | 4096 | Skip offload when the largest array argument's length is below this threshold (avoids H2D/D2H round-trip overhead on tiny inputs). A call below the threshold is *not* blacklisted — a later call at the same site with a bigger array can still offload (`DispatchOutcome::FallThroughKeepHooked`). |
| `--print-gpu-decisions` | off | Log one line per analyzer verdict (`Eligible`/`Rejected(reason)`) and dispatch outcome. Visible without setting `RUST_LOG` — the flag installs its own `tracing` filter directive for the offload module. |
| `--gpu-info` | n/a | Probe the device, print name + compute capability + memory, exit. No JVM bootstrap. |

When `--gpu` is requested but the driver is missing:

```
[cratonvm-cli] --gpu requested but no CUDA driver available (no CUDA driver available …); running on CPU
```

The flag is silently demoted; the program continues on CPU.

Useful environment variables: `CRATONVM_GPU_TRACE_BYTES=1` (per-submit
H2D byte count on stderr — the tell for "offload is actually still
running on CPU because it never touched the device"),
`CRATONVM_GPU_NO_ZEROCOPY=1` (opt out of the zero-copy DMA marshalling
path, falling back to a packed-copy upload for every array).

## Architecture

```
                    ┌──────────────────────────────┐
                    │     vm-cli (--features gpu)   │
                    │  parses --gpu*, plumbs into   │
                    │  VmConfig.gpu_offload_enabled │
                    └────────────────┬─────────────┘
                                     │
                ┌────────────────────▼─────────────────────┐
                │   cratonvm-vm (cfg gpu-offload)            │
                │                                          │
                │  execute_invokestatic() hook ─────────┐  │
                │       │  (offload_jit_gate keeps a    │  │
                │       │   caller with an eligible     │  │
                │       │   invokestatic un-JIT'd, else  │ │
                │       │   the hook stops firing once   │ │
                │       │   OSR promotes the caller)     │ │
                │       ▼                                │ │
                │  runtime::offload::try_dispatch ────┐  │ │
                │       │                              │ │ │
                │       ▼                              │ │ │
                │  OffloadCacheRegistry.get_or_create  │ │ │
                │       │  → OffloadCache.lookup_or_   │ │ │
                │       │    compile (analyze → lower  │ │ │
                │       │    → cuModuleLoad, cached)   │ │ │
                │       ▼                              │ │ │
                │  CompiledKernel ◄─── jit-cuda ◄─────┘ │ │
                │       │                                │ │
                │       │   ┌─── gpu_marshal ────┐       │ │
                │       │   │  host_view_<T> /   │       │ │
                │       │   │  write_back_<T>    │       │ │
                │       │   │  (bulk copy, i8..f64;      │
                │       │   │   zero-copy DMA opt-in)    │
                │       │   └────────────────────┘       │ │
                │       ▼                                │ │
                │  dispatch_method_from_native_on_stream │ │
                │       │  (a registered GpuStream, or   │ │
                │       │   a fresh private stream for   │ │
                │       │   the transparent path)        │ │
                │       ▼                                │ │
                │  cuda-bridge ──── DeviceContext        │ │
                │                   DeviceModule         │ │
                │                   DeviceBuffer<T>      │ │
                │                   Event::query /       │ │
                │                   add_host_callback    │ │
                │       │                                │ │
                │       ▼                                │ │
                │  poll_submission_status /              │ │
                │  finalize_submission                   │ │
                │  (failure-flag drains BEFORE array      │ │
                │   writebacks — no partial GPU state     │ │
                │   reaches the heap on a bounds/div0     │ │
                │   deopt; scalar reduction results       │ │
                │   surface as DispatchOutcome::          │ │
                │   HandledWithValue)                     │ │
                └────────────────────┬───────────────────┘
                                     │
                ┌────────────────────▼─────────────────────┐
                │   cratonvm-gc (cfg gpu-offload)            │
                │   SafepointToken + Heap::pin_ref          │
                │   GC delays while any token is alive      │
                └──────────────────────────────────────────┘
```

## The eight pieces

### 1. `cuda-bridge` (crate)

[`cuda-bridge/`](../../cuda-bridge/)

Thin, JVM-agnostic CUDA Driver API wrapper. **Two backends:**

- `backend_stub.rs` — default. Every fallible call returns
  `DeviceError::NoDriver`. Enables the workspace to build on machines
  without a CUDA toolkit.
- `backend_cuda.rs` — gated by the `cuda` feature. Real bindings via
  the [`cudarc`](https://crates.io/crates/cudarc) crate (pinned to
  `cuda-12060`). Runs a 3-stream context internally (H2D / compute /
  D2H) with per-buffer `last_write` event ordering so `launch_raw` and
  `launch_on_stream` compose safely without manual synchronization.

Public surface:

| Type / fn | Purpose |
| --- | --- |
| `probe() -> Result<DeviceCaps>` | Discover the attached GPU. |
| `DeviceContext::new(ordinal: u32)` | Acquire / attach to a context. |
| `DeviceModule::from_ptx(&ctx, ptx, &[kernel_names])` | Load a PTX text module and resolve named kernels. |
| `DeviceBuffer<T>` | Typed device-side allocation: `uninit`, `zeros`, `from_host`, `to_host`, async H2D/D2H variants. `T: bytemuck::Pod`. |
| `KernelArgs` | Builder pattern: `push_device_ptr`, `push_i32`/`i64`/`f32`/`f64`. |
| `LaunchConfig` + `LaunchConfig::elementwise(n)` / `elementwise_for_kernel` | Grid/block dimensions; the latter queries `cuOccupancyMaxPotentialBlockSize` and is what dispatch actually uses. |
| `Stream` / `Event` | `Event::query()` (non-blocking probe), `Stream::add_host_callback` (`cuLaunchHostFunc` — callback must not call any CUDA API), `record_event`/`wait_event`. |
| `module.launch_raw(&ctx, name, &cfg, args)` / `launch_on_stream` | Kernel launch, sync or stream-scoped. |
| `DeviceError` | `NoDriver` + per-stage variants (`Driver`, `Load`, `KernelNotFound`, `Launch`, `Memcpy`). |

### 2. `jit-cuda` (crate) — Java bytecode → PTX lowering

[`jit-cuda/`](../../jit-cuda/)

Two stages:

- **`analyzer`** (`src/analyzer.rs`): `analyze`/`analyze_with_annotations`/
  `analyze_with_pool`/`analyze_with_annotations_and_pool` →
  `OffloadVerdict`. Static gate that decides GPU eligibility. See "What
  the analyzer accepts" below for the current (much wider than
  originally shipped) opcode/shape coverage.
- **`lowering`** (`src/lowering.rs` + `lowering/loop_recog.rs` +
  `lowering/emit.rs`): consumes the bytecode of an eligible method and
  produces a `PtxModule`.
  - `loop_recog` recognizes the canonical counted-loop pattern
    (`for (i = K; i < bound; i++)`, `K >= 0` constant, unit stride,
    `if_icmpge` exit) — including `ldc`-sourced large `K`. Returns
    `UnsupportedNode` for multiple back-edges, nested loops, non-unit
    strides, or non-`if_icmpge` exits.
  - `emit` walks the bytecode, simulates the JVM operand stack with
    PTX virtual registers, emits PTX text. The `tid` register carries
    `ctaid.x * ntid.x + tid.x + K` (the loop-start offset).

The emitted kernel is SIMT element-wise:

1. Compute `tid = ctaid.x * ntid.x + tid.x`, then add the loop-start
   offset `K` if non-zero.
2. Bounds-check `tid` against the loop bound; if outside, `ret`.
3. Emit the loop body with `iload <iv>` rewritten to `tid` and `iinc
   <iv>` skipped.
4. Every `*aload` / `*astore` carries a bounds check that jumps to a
   shared `L_bounds_fail` label which writes `1` to `*failure_flag`
   (a u64 device buffer) and `ret`s. Integer division/remainder by
   zero routes through the same failure-flag path (predicated guard
   ahead of `div.s32`/`div.s64`/`rem.s32`/`rem.s64`; skippable per-
   kernel via `@GpuKernel(admit = AdmissionHint.ALLOW_DIV_BY_ZERO)`,
   which also gates `frem`/`drem` — see `annotations.md`).
5. A scalar-return (non-void) kernel's epilogue is either a plain
   `st.global` (straight-line methods — every thread would compute
   the same value) or `red.global.add.<suffix>` for a proven
   loop-carried-accumulator reduction (`is_reduction: true`; the host
   pre-zeros the 1-element output buffer before launch).

**Kernel parameter convention** (matches `build_param_list` in
`lowering.rs` and the marshalling layout in `gpu_marshal.rs`):

| Java parameter | PTX slots |
| --- | --- |
| `int` / `long` / `float` / `double` | one `p<i>: .s32/.s64/.f32/.f64` |
| Primitive array | two slots: `p<i>_ptr: .u64`, `p<i>_len: .s32` |
| Scalar (int/long) return, reduction | trailing `ret_ptr: .u64` (one-element output buffer, host pre-zeroed) |
| Scalar return, non-reduction | trailing `ret_ptr: .u64` (every thread writes the same value) |
| Always last | `failure_flag: .u64` |

### What the analyzer accepts

Beyond the original narrow shape, the analyzer now admits:

- **`ldc`/`ldc_w`/`ldc2_w`** of `Integer`/`Float`/`Long`/`Double`
  constant-pool entries (any literal, not just `sipush`-range ints).
  `String`/`Class`/`MethodHandle`/`Dynamic` constants still reject.
- **`frem`/`drem`** — opt-in under `AdmissionHint::AllowDivByZero`
  (reused; the flag now also documents its float-remainder meaning).
  Exact only for `|quotient| < 2^24` (f32) / `2^53` (f64) — see
  `annotations.md` for the reasoning.
- **`lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`** in *value* position
  (JLS-correct -1/0/1, with NaN semantics for the float/double forms).
  A comparison immediately consumed by an `if<cond>` branch (the
  overwhelmingly common javac-emitted shape) still rejects at the
  branch opcode — no general control flow inside a kernel yet.
- **Non-zero loop starts** — `for (int i = K; i < bound; i++)`, any
  non-negative constant `K` (including `ldc`-sourced). Negative `K`
  still rejects (would need more launch-grid threads than the host
  sizes for).
- **A curated `Math`/`StrictMath` intrinsics table** under
  `AdmissionHint::AllowIntrinsicCalls`: `sqrt(D)D`, `abs` (all 4
  numeric types, `MIN_VALUE`-correct for int/long), `min`/`max` (all
  4 types, NaN- and signed-zero-correct for float/double), `fma`
  (float/double). Anything else — `sin`/`cos`/`exp`/`log`/`pow`/etc.
  — is deliberately excluded: PTX's approximate transcendentals don't
  meet Java's relative-error contract.

- **An outer counted loop whose body contains SEQUENTIAL inner loops.**
  The outer loop is the parallel dimension — one thread per iteration —
  and every other back-edge is lowered as a real PTX loop that the
  thread runs itself. This is what makes a per-row reduction
  expressible: `out[i] = sum_j w[i*n + j] * x[j]` has a loop-carried
  accumulator, so its `j` iterations cannot be spread across threads
  the way the 2-D rectangular mapping spreads `(i, j)` pairs.
  Recognized by `loop_recog::classify_outer_parallel_loop`, lowered by
  `Emitter::walk_cfg`; `EligibleRowReduction.java` and
  `EligibleSplitMatmul.java` are the fixtures.
- **`Float.float16ToFloat`** → `cvt.f32.f16`, admitted under
  `AdmissionHint::AllowIntrinsicCalls` like the `Math` table. Exact:
  widening f16 to f32 has no rounding mode to choose, denormals, NaNs
  and infinities included.
- **A bare scalar loop bound** — `for (int i = 0; i < n; i++)` where `n`
  is an unmodified `int` PARAMETER, not an array length. The bytecode is
  identical to a hoisted `arr.length`, so the recognizer tells them apart
  by what defined the local; a parameter the method stores to is still
  rejected, because the grid is sized from the ARGUMENT while the guard
  compares against whatever the local holds. Unlike the `.length` forms
  this is not a free acceptance: the launch grid is otherwise sized from
  the largest array argument and a scalar bound can exceed every one of
  them, so `emitter::WorkBound::ParamScalar` carries the parameter index
  to the dispatch site, which sizes the grid from
  `max(largest array length, that parameter's runtime value)`. A 2-D
  rectangular nest bounded by two scalars is still rejected — its trip
  count is `rows * cols`, a product, and `WorkBound` names one parameter.
  `EligibleScalarBound.java` is the fixture.
- **A ternary lowered without a branch.** `cond ? a : b` becomes two
  unconditional computations and one `selp` when both arms are single
  basic blocks whose emitted PTX holds no label, branch, predicated
  instruction or memory access — which is what makes running both of them
  sound. The screen reads the emitted PTX rather than an opcode list, so
  it cannot drift from the lowering it is judging; an array access is
  caught by its own bounds check's branch. A nested ternary converts its
  inner diamond and keeps the outer branch, and a short-circuit `&&` (two
  conditional branches to one else-label) keeps both. Kernels in this tree
  are written branchlessly on purpose and the lowerer used to turn them
  back into branches, at 18% of the ray tracer's SASS in `BRA` plus
  `BSSY`/`BSYNC`/`BMOV` reconvergence triples. `CRATONVM_GPU_IF_CONVERT=0`
  restores the branching form; `EligibleTernary.java` is the fixture.
- **`Math.exp`** → `ex2.approx.f32` of `x * log2(e)`, and only when
  `CRATONVM_GPU_APPROX_MATH=1` is set. It is the one entry in the
  table that is not bit-exact with the JDK (about 2 ULP against
  `Math.exp`'s 1 ULP in double precision), so it is gated on its own
  rather than riding on the intrinsic hint — a kernel cannot acquire an
  approximate answer by accident. Every other transcendental stays
  rejected.

Still rejected: non-static methods (except `this.field`-only array
refs), synchronized, native/abstract, non-primitive params, general
allocation, arbitrary method calls, non-intrinsic field access, type
checks, `switch`, `throw`, `jsr`/`ret`, reference arrays, `break` or
`return` out of a loop body, irreducible control flow,
`dup2_x1`/`dup2_x2`.

### 3. `vm::runtime::gpu_marshal` (`#[cfg(feature = "gpu-offload")]`)

[`vm/src/runtime/gpu_marshal.rs`](../../vm/src/runtime/gpu_marshal.rs) — JVM heap ↔ device memory.

For each primitive type (`i32`, `i64`, `f32`, `f64`, `i16`, `i8` — all
six now bulk-copy, not element-wise):

- `host_view_<T>(obj, &heap, &SafepointToken) -> Vec<T>` — pack a Java
  primitive array into a host buffer for upload (`copy_nonoverlapping`
  over the heap arena; per-element fallback for non-contiguous
  layouts).
- `write_back_<T>(obj, &mut heap, src, &SafepointToken)` — copy a
  packed host buffer back into a JVM array's storage.
- `upload_obj_<T>` / `download_obj_<T>` (the `direct_xfer!`-generated
  zero-copy path) — DMA straight against the heap arena when
  `zerocopy_enabled()` and `zerocopy_shape_ok()` both hold (opt out via
  `CRATONVM_GPU_NO_ZEROCOPY=1`).

Generic helpers `upload<T>(&ctx, &[T]) -> Result<DeviceBuffer<T>>` and
`download_into<T>(&buf, &mut [T])` wrap `cuda-bridge`. The
`&SafepointToken` parameter is a type-system marker: holding it proves
we are in a no-GC critical section (see Part 6 below).

### 4. `vm::runtime::offload` (`#[cfg(feature = "gpu-offload")]`)

[`vm/src/runtime/offload.rs`](../../vm/src/runtime/offload.rs) — the cache + dispatch hook. ~2,900 lines; the largest single file in the GPU stack.

| Item | Purpose |
| --- | --- |
| `CompiledKernel` | Loaded PTX `DeviceModule` + the `KernelSignature` + the mangled kernel name. |
| `OffloadCacheRegistry` | Per-VM registry of `OffloadCache`s keyed by device ordinal; also owns the `GpuStream` registry (`stream_create`/`stream_release`/`resolve_stream`) backing executor-default-stream affinity. |
| `OffloadCache` | Per-device cache. Lazily probes the device. `lookup_or_compile(class_id, class_name, method_index, &method, &constant_pool) -> LookupOutcome`. |
| `LookupOutcome::{Hit, Skip, Blacklisted}` | `Hit` carries an `Arc<CompiledKernel>`. `Skip`: no device, or (for a later re-check) not yet analyzed. `Blacklisted`: previously rejected; never re-analyzed. |
| `DispatchOutcome::{Handled, HandledWithValue(Value), FallThrough, FallThroughKeepHooked}` | What the interpreter sees from `try_dispatch`. `Handled`/`HandledWithValue` mean the GPU path completed the call — the call site is deliberately **not** promoted into the invoke cache, so the hook keeps firing on the next call. `FallThroughKeepHooked` is the same non-promotion contract for a per-call gate miss (e.g. below `--gpu-min-work`) on an otherwise-eligible kernel. `FallThrough` is a permanent ineligibility verdict. |
| `try_dispatch(shared, thread, frame_idx, class, method, descriptor, args)` | Entry point called from the interpreter hook. |
| `dispatch_method_from_native` / `dispatch_method_from_native_on_stream` | Marshals args, launches (optionally on a caller-supplied `GpuStream` handle), registers a `StreamSubmission`. |
| `poll_submission_status(shared, handle) -> Option<PollOutcome>` | Non-blocking completion probe (`Event::query`; finalizes inline when the device reports done). Backs `Native.futureIsDone`/`futureStatus`. |
| `finalize_submission` | Blocking finalize (`event.synchronize()`), used by `get()`. Drains the `FailureFlag` writeback **before** any array writeback — a bounds/div-by-zero deopt never lets partial GPU state reach the heap. |
| `output_array_index(&param_kinds)` | The *last* array parameter is the output sink (matches `EligibleVectorAdd(a, b, out)`). |

### 5. `vm::runtime::offload_jit_gate` (`#[cfg(feature = "gpu-offload")]`)

[`vm/src/runtime/offload_jit_gate.rs`](../../vm/src/runtime/offload_jit_gate.rs) — keeps offload-eligible **callers** interpreted.

The transparent-offload hook only fires from the interpreter's
`execute_invokestatic` slow path. If the *caller* method itself gets
JIT/OSR-compiled, dispatch moves into JIT-emitted code and the hook is
never consulted again for that call site. `caller_blocks_jit`/
`caller_blocks_jit_by_name` scan a caller's bytecode for an
`invokestatic` whose target resolves `Eligible` (plain, annotation-free
verdict) and, if found, deny JIT/OSR admission for that caller while
`--gpu` is active — wired into all five JIT/OSR admission call sites in
`interpreter.rs` (first-call compile, OSR, and three invocation-count
promotion paths). Verdicts are cached per `(ClassId, method_index)` for
the process lifetime; one `bool` read (`gpu_offload_enabled`) when
`--gpu` is off.

### 6. `gc::safepoint` (`#[cfg(feature = "gpu-offload")]`)

[`gc/src/safepoint.rs`](../../gc/src/safepoint.rs) + additions to [`gc/src/heap.rs`](../../gc/src/heap.rs).

| Item | Purpose |
| --- | --- |
| `SafepointToken<'h>` | Zero-sized, `!Send`, RAII. Drop decrements an atomic on the heap. Construction is gated through `Heap::enter_gpu_critical()`. |
| `Heap::enter_gpu_critical() -> SafepointToken<'_>` | Acquire a token. Increments `gpu_critical_count`. |
| `Heap::pin_ref(obj)` / `unpin_ref(obj)` | Mark/unmark a JVM `ObjectRef` as a GC root for the duration of a kernel. |
| GC delay inside `collect_garbage` / `collect_garbage_with_finalizers` | While `gpu_critical_count > 0` the collector yield-spins (warns after 5s; **never** collects mid-token). |
| `gpu_critical_count()` / `gpu_blocked_gc_count()` | Observability accessors. |

The root walker visits pinned refs during collection so the JVM cannot
move an array while a kernel reads it. For the cross-thread deferred-
finalize path (async futures), a process-wide `GPU_CRITICAL_COUNT` plus
a `Send`-able `GcCriticalGuard` extends the same guarantee past the
dispatching thread's stack frame.

### 7. `native-api` / `native-builtins::craton_gpu` — the Java-facing bridge

[`native-api/src/registry.rs`](../../native-api/src/registry.rs) (the
`NativeContext` trait: `gpu_future_status`, `gpu_future_take_result`,
`gpu_stream_create`/`gpu_stream_release`, `gpu_dispatch_method_on_stream`
— all default-`None`/no-op so every other `NativeContext` implementor
in the workspace is unaffected) and
[`native-builtins/src/craton_gpu.rs`](../../native-builtins/src/craton_gpu.rs)
(the `craton.gpu.*` native shims: `submitMethod`, `futureIsDone`/
`futureGetResult` (now boxes real `Integer`/`Long`/`Float`/`Double`
reduction results via `GpuFutureResult`), `newStream`/`closeStream`,
`arrayWrap*`/`arrayAllocate*`/`arrayToHost`/`releaseArray`). This is the
only layer `native-builtins` can use to reach vm-crate GPU state — it
has no Cargo dependency on `cratonvm-vm`.

### 8. `jit-api::gpu_lowering` (`#[cfg(feature = "gpu-lowering")]`)

[`jit-api/src/gpu_lowering.rs`](../../jit-api/src/gpu_lowering.rs) — a
single optional trait, `GpuLowering`, defining the contract a future
pluggable PTX producer would satisfy. There is no in-workspace
implementor: `jit-cuda` exposes its concrete analyzer/lowering entry
points directly and does not enable `jit-api/gpu-lowering`. The trait
exists so that a future Rust-authored device-helper story (parallel GC
mark, atomic intrinsics via cuda-oxide) could plug in without touching
`try_dispatch`. See [`cuda-oxide-evaluation.md`](cuda-oxide-evaluation.md).

## How a method actually offloads (annotated walk)

`EligibleVectorAdd.vectorAdd(int[] a, int[] b, int[] out)`, `n = 1<<20`,
built with `--features gpu-driver`, run with `--gpu` on a real GPU:

1. The interpreter is about to dispatch `invokestatic vectorAdd`. The
   cfg-gated hook fires (the call site is not in the invoke cache —
   see Part 4's `DispatchOutcome` note).
2. `try_dispatch` resolves the class, finds the method index, calls
   `OffloadCache::lookup_or_compile`.
3. First call: cache miss. The analyzer says
   `Eligible(KernelSignature{ param_kinds: [I32Array, I32Array, I32Array], return_kind: Void, estimated_work: 1<<20, is_reduction: false, ... })`.
4. `jit_cuda::lower_method_with_pool` produces a PTX module (pool-aware,
   for `ldc` support). `DeviceModule::from_ptx` loads it; the
   `Arc<CompiledKernel>` is cached under `(ClassId, method_index)`.
5. `try_dispatch` checks the void-return-or-integer-reduction gate and
   the `--gpu-min-work` gate against the *runtime* array length (not
   the analyzer's fixed `estimated_work` placeholder), then calls
   `dispatch_method_from_native`.
6. **Launch**, done today: acquire `Heap::enter_gpu_critical()`; for
   each array argument, `host_view_i32`/zero-copy DMA upload, push a
   device pointer + length into `KernelArgs`, pin the `ObjectRef`;
   allocate the trailing `failure_flag` buffer; launch via
   `LaunchConfig::elementwise_for_kernel` (occupancy-tuned, sized off
   the actual runtime array length) on either a registered `GpuStream`
   or a fresh private stream; record a completion `Event`.
7. **Finalize**: either `poll_submission_status` (non-blocking, driven
   by `futureIsDone`) or `finalize_submission` (blocking, driven by
   `get()`/the transparent path) reads `failure_flag` first — non-zero
   means a bounds or div-by-zero trap inside the kernel, and the call
   deopts to the interpreter/JIT with **no partial GPU state written**
   to the heap. Zero means success: array writebacks run, a reduction's
   scalar result (if any) is read back and surfaces as
   `DispatchOutcome::HandledWithValue`, the `SafepointToken`/pins are
   released, and the interpreter gets `Handled`.
8. Subsequent calls at the same site skip analyze/lower (cache hit) and
   the invoke cache is never populated for this site (Part 4), so step
   1 repeats on every call — this is intentional: an eligible site
   whose array size varies per call must keep re-checking
   `--gpu-min-work`.

## Float bit-exactness

The offload contract is that `--gpu` never changes a result. For
floating-point kernels that is a claim about *rounding*, and it is easy
to lose without noticing, because the ways to lose it all make the answer
**more** accurate rather than visibly wrong.

Every float arithmetic instruction the lowerer emits therefore carries an
explicit `.rn` rounding modifier — `add.rn.f32`, `sub.rn.f32`,
`mul.rn.f32`, `div.rn.f32`, and the `f64` twins. This is not decoration:

* PTX's bare `add.f32` / `mul.f32` already *default* to
  round-to-nearest-even, so the modifier looks redundant. It is not. Per
  the PTX ISA, an instruction written without a rounding modifier is
  eligible for **contraction**, and one written with a modifier is not.
  Measured on sm_75 with `ptxas -O3`, `mul.f32` followed by `add.f32`
  assembles to a single `FFMA`; `mul.rn.f32` followed by `add.rn.f32`
  assembles to `FMUL` + `FADD`.
* `FFMA` rounds once. JLS §15.17.1/§15.18.2 require the product to be
  rounded to `float` *before* the addition — two roundings. Java spells
  the single-rounding form `Math.fma`, which the lowerer emits as
  `fma.rn.f32`. Fusing is the programmer's call, never the backend's.
* `div` must be `div.rn` (correctly rounded), not `div.full` or
  `div.approx`, which carry up to ~2 ULP of error.
* `Math.min`/`Math.max` are **not** PTX `min.f32`/`max.f32`. Those
  implement neither Java's "NaN result if either argument is NaN" rule
  nor its "−0.0 is strictly smaller than +0.0" rule, so the lowerer
  builds both out of ordered predicates and selects (`minmax_f32`).
* A float square root lowers to **one `sqrt.rn.f32`**, not to an f64
  square root with a widen and a narrow around it. `java.lang.Math`
  declares square root only as `sqrt(D)D`, so `(float) Math.sqrt(f)` is
  the only way to write one and always compiles to
  `f2d; invokestatic sqrt(D)D; d2f`. Lowering that literally runs the
  whole thing in double precision, which on a consumer GPU costs 32x —
  Turing's FP64 rate. Collapsing it is exact rather than an
  approximation: double rounding of a square root is innocuous when
  `p64 >= 2*p32 + 2`, and 53 >= 50. That was verified over all 2^32
  float bit patterns rather than cited. Only the complete triple
  collapses — a genuine `double` square root, or one whose result is
  kept as a double, still emits `sqrt.rn.f64`. See
  `float_sqrt_triple_at`.

The contraction hazard needs an `a*b + c` chain to surface at all — a kernel
with no mul-then-add pair to contract never exercises it, which is why it can
stay latent across fixtures that happen not to contain one. The regression
test
`float_arithmetic_always_carries_an_explicit_rounding_mode` asserts on
the rendered PTX text, so it runs everywhere and does not need a CUDA
toolkit.

To inspect what a kernel actually compiled to, set
`CRATONVM_GPU_DUMP_PTX=<dir>`; each lowered kernel is written to
`<dir>/<class>.<method>.ptx`.


## Testing

| Crate / target | Tests | What they prove |
| --- | --- | --- |
| `cargo test -p cratonvm-jit-cuda` | 110 (some `#[ignore]`d without `gpu-it`) | Real `.class` fixtures (32 sources) lower to correct PTX for every accepted shape; every `Reject*` fixture classifies correctly; `ptxas` round-trip tests (feature `gpu-it`) assemble the emitted PTX for vector-add, dot-reduction, `ldc` constants, offset loops, `frem`, and Math intrinsics through the real NVIDIA assembler. |
| `cargo test -p cratonvm-cuda-bridge` | ~29 (stub-mode; `--features cuda` compiles the real backend too, gated in CI) | `NoDriver` contract; `Event`/`Stream` state machines; `launch.rs`'s H2D→kernel→D2H event-ordering discipline; stub op log. |
| `cargo test -p cratonvm-vm --features gpu-offload --lib gpu_marshal` | 20 | Round-trip every primitive array type (incl. bulk i16/i8) through a real `Heap`, including odd lengths, zero length, and G1-humongous scale. |
| `cargo test -p cratonvm-vm --features gpu-offload --lib offload` | 17 | Cache skip/blacklist behavior on no-device, void/reduction/min-work gating, submission registry lifecycle, `poll_submission_status`. |
| `cargo test -p cratonvm-vm --features gpu-offload --lib offload_jit_gate` | 13 | Bytecode `invokestatic`-scanner correctness (including adversarial byte sequences inside `tableswitch`/`wide`), fail-open on unresolvable methods, config-off short-circuit. |
| `cargo test -p cratonvm-vm --features gpu-offload --test gpu_offload_features` | 21 (3 `#[ignore] = "requires NVIDIA GPU"`) | Integration-level coverage through a real `SharedVm`; the 3 ignored tests are the ones the CI gate (`ci.md`) runs on real hardware. |
| `cargo test -p cratonvm-native-builtins --features gpu-offload craton_gpu::` | 43 | Java-facing shim behavior: array wrap/allocate/toHost round-trips, future status/result boxing (incl. the four scalar types), stream create/release idempotency. |
| `cargo test -p cratonvm-gc --features gpu-offload` | 66 GPU-specific (+ the full non-GPU suite unaffected) | Token increment/decrement, nested tokens, GC blocks while a token is held, pinned refs survive a real GC cycle, root walker visits pinned refs. |

**All of the above run on machines without an NVIDIA GPU** (device-
requiring paths self-skip). Real-hardware verification is the 3
`#[ignore]`d tests plus the manual/CI-gate benchmark suite in
`bench-gpu/` — see [`ci.md`](ci.md) and the repo README.

### No synthetic stubs

Every test fixture is a real Java source compiled by `javac` via
[`jit-cuda/build.rs`](../../jit-cuda/build.rs). There are zero hand-
rolled bytecode arrays in the fixture set (a few *lowering* unit tests
do drive the `Emitter` directly with hand-built bytecode arrays for
white-box PTX-shape assertions — a different, accepted pattern; see
`jit-cuda/src/lowering.rs`'s test module). The 32 fixtures in
[`test_classes/gpu/`](../../test_classes/gpu/) cover eligible shapes
(vector-add, saxpy, dot-product reduction, `ldc` constants of all four
types, non-zero loop starts, `frem`, Math intrinsics, comparison-in-
value-position), every `Reject*` analyzer reason, non-canonical control
flow, the two bounds-deopt regression fixtures (`BoundsDeopt2`/`3`),
and a GC stress program (`GcStress`).

## Known follow-ups

All hardware-validation follow-ups from the first real-hardware pass are
closed. Summary:

- **DONE**: reduction dispatch (int/long only — see "What the analyzer
  accepts"), the JIT-caller admission gate, the launch-config thread-
  floor/occupancy fix, `ldc`/`frem`/cmp/non-zero-loop-start/Math-
  intrinsics opcode coverage, `--print-gpu-decisions` visibility,
  self-hosted GPU CI scaffolding (runner enrollment still pending).
- **DONE**: asynchronous completion reaping, 2-D rectangular loops, and
  acyclic `if`/`else` loop-body CFG lowering with join-state reconciliation.
- **Intentional CPU fallback**: float (`)F`/`)D`) reductions (GPU atomic-add
  reorders summation versus Java's sequential FP semantics), early exits, and
  interior backward branches in a per-iteration kernel.

## File index (just the GPU-touching files)

```
cuda-bridge/                         Optional dep of vm-cli / cratonvm-embed.
  Cargo.toml, README.md
  src/
    lib.rs                           Public API + stub-mode tests
    backend_cuda.rs                  cudarc backend (feature = "cuda")
    backend_stub.rs                  No-driver backend (default)
    stream.rs, event.rs, launch.rs   Stream/Event, host callbacks, launch choreography
  tests/stub_op_log.rs               Stub-mode integration tests

jit-cuda/                            Used by cratonvm-vm under gpu-offload.
  Cargo.toml, build.rs               build.rs compiles test_classes/gpu/*.java
  src/
    lib.rs
    analyzer.rs                      OffloadVerdict, ParamKind, Reason, MathIntrinsic table
    annotations.rs                   @GpuKernel / AdmissionHint / @GpuExclude / @EnableGpuAsync
    emitter.rs                       PtxModule / PtxKernel / RegKind
    signature.rs                     KernelSignature
    lowering.rs                      lower_method / lower_method_with_pool entry points
    lowering/loop_recog.rs           Canonical counted-loop recognizer (incl. offset starts)
    lowering/emit.rs                 Opcode walker + register allocator
    test_support.rs                  load_method(class, method, descriptor)

jit-api/
  Cargo.toml                         gpu-lowering feature
  src/lib.rs                         Conditionally exposes gpu_lowering module
  src/gpu_lowering.rs                GpuLowering trait + LoweredKernel

gc/
  Cargo.toml                         gpu-offload feature
  src/safepoint.rs                   SafepointToken
  src/heap.rs                        Gated fields + enter_gpu_critical + pin_ref + GC delay
  src/vm_heap.rs                     Process-wide GPU_CRITICAL_COUNT for cross-thread finalize

native-api/
  src/registry.rs                    NativeContext GPU trait methods + GpuFutureResult

native-builtins/
  Cargo.toml                         gpu-offload feature
  src/craton_gpu.rs                  craton.gpu.* native shims

vm/
  Cargo.toml                         gpu-offload feature, optional cuda-bridge / jit-cuda / bytemuck
  src/config.rs                      Gated VmConfig fields (gpu_offload_enabled, ...)
  src/runtime/mod.rs                 Gated module declarations
  src/runtime/gpu_marshal.rs         Heap ↔ device marshalling
  src/runtime/gpu_residency.rs       GpuArray host-byte residency tracker
  src/runtime/offload.rs             OffloadCacheRegistry + OffloadCache + try_dispatch + poll
  src/runtime/offload_jit_gate.rs    JIT/OSR admission gate for offload-eligible callers
  src/runtime/interpreter.rs         Gated hook in execute_invokestatic + 5 JIT-gate call sites
  src/vm/vm_init.rs                  offload_registry field on SharedVm
  src/vm/vm_exec.rs                  NativeContext GPU trait impls
  tests/gpu_offload_features.rs      Integration tests (stub-mode + #[ignore]d hardware tests)

vm-cli/
  Cargo.toml                         gpu / gpu-driver features, optional cuda-bridge
  src/main.rs                        Gated --gpu* flags + --gpu-info early-exit + tracing filter

cratonvm-embed/
  Cargo.toml                         Mirrors vm-cli's gpu / gpu-driver features for embedders

craton-gpu/                          Build-time only: packages @GpuKernel/@Parallel annotation
                                      sources from an external craton-gpu-java checkout.

test_classes/gpu/
  README.md                          The "no synthetic stubs" rule
  *.java / *.class                   32 real Java fixtures (see Testing above)

bench-gpu/, bench-tornado/           Benchmark sources + TornadoVM twins; results in bench-gpu/results/.
.github/workflows/gpu-selfhosted.yml Self-hosted GPU CI workflow.

docs/gpu/                            This directory.
docs/book/src/gpu/                   User-facing book chapter + benchmarks page.

Cargo.toml                           cuda-bridge and jit-cuda as workspace members
Cargo.lock                           Locked cudarc + bytemuck transitives
```

## FAQ

**Q: Why feature flags instead of a runtime config flag?**
A: A runtime flag would leave dead branches in the interpreter hot
path. The `#[cfg(feature = "gpu-offload")]` discipline means the
compiled CPU binary contains *no* GPU code at all — easier to audit and
impossible to regress accidentally.

**Q: Why not use cuda-oxide directly?**
A: See [`cuda-oxide-evaluation.md`](cuda-oxide-evaluation.md). Short
version: cuda-oxide compiles *Rust source* to PTX. Our problem is *Java
bytecode* to PTX. cuda-oxide doesn't help with that path. It might be
useful much later for Rust-authored device helpers (parallel GC mark,
atomic intrinsics) — the `GpuLowering` trait keeps that door open
without a hard dependency today.

**Q: What happens to a Java exception thrown inside an offloaded method?**
A: `ArrayIndexOutOfBoundsException`-equivalent bounds failures and
integer divide-by-zero (`ArithmeticException`) are both handled: the
kernel writes `1` to `*failure_flag` and `ret`s; the host detects this
(draining the failure flag **before** any array writeback) and deopts
to the interpreter, which observes no partial GPU state and re-runs
with correct Java semantics — validated on hardware
(`test_classes/gpu/BoundsDeopt2.java`/`BoundsDeopt3.java`). Any other
Java exception means the method isn't actually offload-eligible — the
analyzer rejects it (calls, `throw`, etc. are all rejected shapes).

**Q: How do I add support for a new opcode?**
A: Three places.
1. Make sure `jit-cuda/src/analyzer.rs::classify()` admits the opcode
   (or doesn't reject it for the wrong reason).
2. Add the emission case in `jit-cuda/src/lowering/emit.rs`.
3. Add a real `.java` fixture under `test_classes/gpu/` that uses the
   opcode, an analyzer test, and a lowering test that asserts the PTX
   content contains the right instruction (and, if hardware is
   available, a `ptxas` round-trip test — see the several examples in
   `jit-cuda/src/lowering.rs`).

**Q: How do I add a new GPU lowering backend (e.g. cuda-oxide-emitted helpers)?**
A: Implement the `jit_api::gpu_lowering::GpuLowering` trait. Wire it
into `OffloadCache::new`'s lowering chain. The interpreter hook never
has to change.

**Q: I enabled `--gpu` and my kernel still runs on CPU. Why?**
A: Run with `--print-gpu-decisions` (no `RUST_LOG` needed) and check
the analyzer verdict. Common reasons: the method isn't in the accepted
shape (see "What the analyzer accepts" above — check the `Reason`);
the largest array argument is below `--gpu-min-work` (default 4096);
the kernel has a non-void, non-integer-reduction return (float/double
reductions and non-reduction non-void kernels still run on CPU); or the
caller itself got JIT-compiled before the offload-eligible target was
first analyzed (rare — the JIT gate should prevent this once the
target has been seen once; file a bug if you hit it).
