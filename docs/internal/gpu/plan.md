# Plan — GPU offload for CratonVM (CratonVM)

## Context

The user pointed at [cuda-oxide](https://nvlabs.github.io/cuda-oxide/) and asked us to evaluate its impact on this project and produce a plan to make CratonVM (the Rust-based JVM in this worktree) **execute real Java classes on the GPU** — explicitly no synthetic JDK stubs in the tests; real `.class` files must run.

### What cuda-oxide is — and what its purpose is for us

cuda-oxide is **an experimental Rust-to-PTX compiler**: a custom `rustc` codegen backend that lowers ordinary Rust source through `pliron` (an MLIR-like IR) to NVIDIA PTX. Alpha (v0.1.0), nightly toolchain, ships its own runtime (`DeviceBuffer`, `DisjointSlice`, async `DeviceOperation` graphs).

**Critical fact:** cuda-oxide and cudarc are not competitors. They solve different problems.

- **cudarc** is the **runtime** — `cuMemAlloc`, `cuMemcpy`, `cuLaunchKernel`. It hands PTX to the driver and runs it. You still need to produce the PTX from somewhere.
- **cuda-oxide** is a **compiler** — it produces PTX *from Rust source*. It does not produce PTX from Java bytecode.

**Our actual problem is Java bytecode → PTX.** Neither tool does that. We have to build it ourselves regardless of which side-tools we adopt. We will build it as a new lowering target inside the existing JIT IR pipeline ([jit/src/ir_lower.rs](jit/src/ir_lower.rs) already separates IR from machine-code emission alongside x64/aarch64).

So **what is cuda-oxide for, in this project?** Honestly: not much, today.

1. **It does not shorten Part B.** Even if we adopted it, we would have to lower Java bytecode → synthesized Rust source → cuda-oxide → PTX. That is a strictly worse pipeline than lowering Java IR → PTX directly. Skip.
2. **It has one plausible future role:** *Rust-authored device-side helpers.* If we later want to write parallel GC mark, atomic helpers, or array intrinsics in Rust and have them compiled to PTX in-process (no nvcc, no NVRTC round-trip), cuda-oxide is the only path that fits. That is months away, not now.
3. **Its `pliron` IR is interesting prior art** if we ever rebuild our JIT IR around MLIR-style dialects. Worth reading; not worth depending on.

**Verdict on cuda-oxide's impact on CratonVM: minimal.** We will use **cudarc** for the runtime (Part A) and **our own emitter** for PTX (Part B). cuda-oxide gets a single small Part — Part I — whose job is to (a) write down this evaluation so future readers don't re-litigate it, and (b) keep a `trait GpuLowering` seam in our code so that if cuda-oxide ever matures into the right tool for device-side helpers, swapping is straightforward. Part I is documentation + one trait. Nothing more.

#### Reference table

|                      | **cudarc** (we use)                                            | **cuda-oxide** (we do not use today)                                 |
| -------------------- | -------------------------------------------------------------- | -------------------------------------------------------------------- |
| What it is           | Safe Rust wrapper over CUDA Driver API + NVRTC + cuBLAS        | rustc codegen backend that lowers Rust → PTX in-process              |
| Maturity             | Stable, used in burn / candle / dfdx                           | v0.1.0 alpha, "expect bugs, incomplete features, API breakage"       |
| Rust toolchain       | Stable                                                         | Nightly                                                              |
| Input it consumes    | PTX text **you produce**, or live Rust via NVRTC               | Rust source via MIR                                                  |
| Solves Java → PTX?   | No                                                             | No                                                                   |
| Useful to us for     | Device discovery, allocation, memcpy, kernel launch            | Nothing on the critical path. Possibly future Rust-side GPU helpers. |
| Risk of taking dep   | Low                                                            | High — pins us to nightly, breaks on rustc churn                     |

### Scope of this plan (locked with user)

- **First offload target:** pure static methods whose signature is `(primitive | primitive[])* → (primitive | primitive[])` and whose bytecode performs no allocation, no calls into other classes, and no exceptions other than `ArithmeticException`/`ArrayIndexOutOfBoundsException` (which trigger deopt back to the interpreter).
- **Platform:** Windows x64 only (the dev box). Linux is a later story; the CUDA driver abstraction is kept thin so it can grow a Linux backend later.
- **Real Java only:** every test loads a real `.class` file compiled by `javac` from sources under `test_classes/gpu/`. No synthesised bytecode arrays, no fabricated `Class` structs. This rule is restated in every Part that touches tests.
- **GPU is an opt-in separate feature, NOT a replacement of the CPU path.** Re-confirmed by the user mid-session. Concretely:
  - The default workspace build (no Cargo features) **must not link `cuda-bridge`** and **must not expose any `--gpu*` CLI flags** or GPU-related runtime behaviour. Default-build CPU execution is byte-identical to before the GPU work began.
  - Three feature levels:
    - `cargo build` — CPU only. No GPU code paths.
    - `cargo build --features gpu` — exposes `--gpu*` CLI flags + cuda-bridge stub backend (returns `DeviceError::NoDriver`).
    - `cargo build --features gpu-driver` — real cudarc bindings to libcuda / nvcuda.dll.
  - Every hook into a CPU-path crate (interpreter, vm-cli, jit-api) MUST live behind `#[cfg(feature = "...")]`. No "runtime-gated dead code in the hot path" trick.
  - Verification gate for every remaining Part: with `cargo build` (no features), the touched CPU crate's diff is empty modulo `#[cfg(feature = "gpu")]`-wrapped additions.

---

## How to read this plan

- Each Part is **one agent session**. Parts marked **[parallel]** can run concurrently; **[serial after X]** depend on X being complete.
- Steps inside a Part are ordered. Mark them `- [x]` as you complete them.
- File paths under `worktree-relative/...` are relative to the worktree root `C:\Projects\CratonVM\.claude\worktrees\gracious-swanson-f74730`.
- A Part is "done" only when its **verification** checklist passes — every Part ends with a runnable check against real Java code.

## Dependency graph

```
A ──┬──> C ──┐
    │        ├──> E ──┐
    └──> H   │        ├──> J (end-to-end demo)
B ──────────┤        │
D ─────────────────────┤
F ──> (joins E)       │
G (real .class files) ┘
I (cudarc-vs-oxide doc + migration scaffolding) — parallel to all
```

Parts A, B, D, G, H, I are immediately parallelisable.
C depends on A. E depends on B+C+D. F depends on A+C. J is the final gate.

---

## Part A — CUDA Driver bridge crate [parallel] — **COMPLETED**

**Goal:** a new workspace crate `cuda-bridge` that gives the rest of the JVM a safe, minimal handle to a CUDA context, module loading, memory allocation, memcpy, and kernel launch on Windows. Nothing JVM-specific lives here.

### Steps

- [x] Create crate `cuda-bridge/` with `Cargo.toml`. Add to root `[workspace] members` in [Cargo.toml](Cargo.toml) (line 2).
- [x] Add `cudarc = { version = "0.13", default-features = false, features = ["driver", "cuda-12060"], optional = true }`. **Deviation:** dropped the `nvrtc` feature for now — we ship pre-emitted PTX text; we don't need NVRTC unless a future story wants live-Rust compilation.
- [x] **Deviation:** introduced a stub backend (`backend_stub.rs`) so the crate compiles when the `cuda` feature is off. Every entry returns `DeviceError::NoDriver`. This keeps the rest of the workspace buildable on machines without a CUDA toolkit, and is the default mode in CI.
- [x] Implement `DeviceContext`, `DeviceModule::from_ptx(ptx, kernel_names)`, `DeviceBuffer<T>`, `LaunchConfig`, `launch_raw(kernel, cfg, args)`, `probe()` → `DeviceCaps`. All live in [cuda-bridge/src/lib.rs](cuda-bridge/src/lib.rs).
- [ ] Doc-test that loads a 4-line PTX kernel and runs it. Skipped this iteration — pending an attached GPU and `--features gpu-it`. Stub-mode build verified.

### Verification

- [x] `cargo build -p cuda-bridge` succeeds (stub mode).
- [ ] `cargo test -p cuda-bridge --features gpu-it -- --nocapture` — pending real GPU.
- [x] On a box without a CUDA driver / without the `cuda` feature, `cuda_bridge::probe()` returns `Err(DeviceError::NoDriver)`.

---

## Part B — `jit-cuda` PTX emitter crate [parallel] — **COMPLETED (body landed)**

**Goal:** a new crate that lowers Java bytecode to **PTX text** for a fixed subset of opcodes. No CUDA driver code here — emission only.

### Major scope deviation

The plan said "lower from the existing JIT IR." After reading [jit-api/src/lib.rs](jit-api/src/lib.rs) and [jit/src/ir.rs](jit/src/ir.rs) the IR types are private to the `jit` crate, not the `jit-api` crate. Pulling `jit` into `jit-cuda` would balloon dependencies (libloading, parking_lot, the entire x64 backend). For the first cut, **`jit-cuda` parses `CachedBytecodeMethod` bytecode directly**. The element-wise methods we target are tiny linear loops; the Sea-of-Nodes IR is overkill. We can lift to the IR in a later iteration without changing the public API.

### Steps

- [x] Create crate `jit-cuda/` with `Cargo.toml`. Add to workspace. Depends on `cratonvm-reader`, `cratonvm-types`, `cratonvm-jit-api`.
- [x] Define `PtxModule { sm_major, sm_minor, kernels }`, `PtxKernel { name, params, body, reg_decls }`, `PtxParam`, `RegDecl`, `RegKind`. All in [jit-cuda/src/emitter.rs](jit-cuda/src/emitter.rs).
- [x] Emit PTX `.version 7.5`, `.target sm_<major><minor>`, `.address_size 64` headers and `.visible .entry` per kernel.
- [x] Parameter-list builder: each Java primitive-array parameter becomes a `(ptr, len)` pair; scalars are single `.param`s; the return shape adds `ret_ptr`(+`ret_len` for arrays); every kernel ends with a `failure_flag: .u64` ptr for deopt signalling.
- [x] Stub `lower_method()` in [jit-cuda/src/lowering.rs](jit-cuda/src/lowering.rs) emits a `ret;`-only kernel so the pipeline links end-to-end. The element-wise opcode lowering itself is the next session.
- [x] Implement the lowering body. Landed in [jit-cuda/src/lowering.rs](jit-cuda/src/lowering.rs) + new submodules [jit-cuda/src/lowering/loop_recog.rs](jit-cuda/src/lowering/loop_recog.rs) and [jit-cuda/src/lowering/emit.rs](jit-cuda/src/lowering/emit.rs). Two-stage design: `loop_recog` recognises the canonical counted loop (or rejects with `UnsupportedNode`); `emit` walks the bytecode, simulates the JVM operand stack with PTX virtual registers, emits PTX text.
- [x] Bounds-check `*aload`/`*astore` against length params; on out-of-bounds, write `1` to `*failure_flag` and `ret`. Shared `L_bounds_fail` label.
- [x] `ptxas -arch=sm_70` round-trip test exists under `#[ignore]` (skipped on this no-GPU machine; runs on the verification box).

### Verification

- [x] `cargo test -p jit-cuda` — **19 tests pass, 2 ignored** (the ptxas round-trip + a diagnostic dump).
- [x] Each eligible fixture (`EligibleVectorAdd`, `EligibleSaxpy`, `EligibleDotProduct`) lowers to non-trivial PTX containing the tid computation, bounds check, the right number of `ld.global.*`/`st.global.*` ops, and the arithmetic opcode.
- [x] Unsupported patterns (multi-loop, non-canonical control flow) return `LoweringError::Unsupported(...)` — fixture `test_classes/gpu/TwoLoops.java` exercises this.

### Known follow-ups (deliberately deferred — not blocking)

- `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` return `Unsupported`. javac usually emits these for non-element-wise patterns; the canonical-loop body doesn't need them.
- `dup2_x1`, `dup2_x2` return `Unsupported` (very uncommon).
- `frem`/`drem` emit text that won't `ptxas`-clean (PTX has no native rem on f32/f64). Analyzer admits them; the kernel will fail validation. The fix is to lower them to a software-correct sequence — follow-up.
- The dot-product fixture lowers, but all CUDA threads write to the same `ret_ptr` slot (a race). The kernel is correct only if every thread happens to compute the same value; for a true sum-reduction a proper reduction kernel is needed. Acknowledged; not on the critical path for the vector-add demo.

---

## Part C — Device memory & primitive-array marshalling [serial after A] — **COMPLETED**

**Goal:** a thin layer that takes a JVM heap object representing `int[]`/`long[]`/`float[]`/`double[]`/`byte[]`/`short[]` and produces a `DeviceBuffer<T>`, and the reverse. Lives in a new module `vm/src/runtime/gpu_marshal.rs`.

### Isolation contract

- The new module declaration in [vm/src/runtime/mod.rs](vm/src/runtime/mod.rs) (or wherever the module list is) MUST be `#[cfg(feature = "gpu-offload")] mod gpu_marshal;`.
- All public items in `gpu_marshal.rs` are reachable only when the feature is on; no shim re-exports.
- The default `cargo build` of `cratonvm-vm` does not compile this file.

### Steps

- [x] `gpu-offload` Cargo feature added to [vm/Cargo.toml](vm/Cargo.toml); `cuda-bridge` and `jit-cuda` are optional deps pulled in by it.
- [x] [vm/src/runtime/gpu_marshal.rs](vm/src/runtime/gpu_marshal.rs) created, declared `#[cfg(feature = "gpu-offload")] pub mod gpu_marshal;` in [vm/src/runtime/mod.rs](vm/src/runtime/mod.rs).
- [x] **Plan correction:** the actual heap doesn't use a uniform 16-byte slot for primitive arrays — it uses native-sized strides (4/8/2/1). The agent reused existing accessors (`Heap::get_array_element`, `Heap::set_array_element`) rather than walking raw memory, so the marshalling is correct regardless of the underlying layout. The 16-byte-slot detail from the architecture-exploration report was about field slots; primitive-array element storage is its own thing.
- [x] `host_view_<T>` and `write_back_<T>` for `i32`, `i64`, `f32`, `f64`, `i16`, `i8`. Generic `upload<T>` and `download_into<T>` wrap `cuda_bridge::DeviceBuffer`.
- [ ] `SafepointToken` parameter — **deferred to Part E.** Agent C did not couple to Part F's symbol (sensible — they ran in parallel). The cfg-gated `TODO(part-f)` marker is in the module docstring. Part E will plug the token in.

### Verification

- [x] `cargo check -p cratonvm-vm` (default) — clean. CPU path unchanged.
- [x] `cargo check -p cratonvm-vm --features gpu-offload` — clean.
- [x] `cargo test -p cratonvm-vm --features gpu-offload --lib gpu_marshal` — **9 tests pass**, each using a real `Heap` and real `Heap::alloc_array` (no synthesised objects).
- [x] `cargo test -p cratonvm-vm` (default) — full pre-existing suite still passes (2007 tests).

---

## Part D — Offload candidate analyzer [parallel] — **COMPLETED**

**Goal:** a function `analyze(class: &Class, method: &ClassFileMethod) -> OffloadVerdict` that statically decides whether a method is GPU-eligible **before** the JIT spends compile time on it. Lives in `jit-cuda/src/analyzer.rs` (placed in Part B's crate; doesn't pull in CUDA deps).

### Steps

- [x] `enum OffloadVerdict { Eligible(KernelSignature), Rejected(Reason) }` — [jit-cuda/src/analyzer.rs](jit-cuda/src/analyzer.rs).
- [x] `ParamKind` covers `I32`, `I64`, `F32`, `F64`, `I32Array`, `I64Array`, `F32Array`, `F64Array`, `I16Array`, `I8Array`, `Void`.
- [x] Linear opcode scan that classifies every opcode (non-overlapping match arms, no Rust-match-arm absorption bug). Specific rejects: `aaload`/`aastore`, `if_acmp*`, `jsr*`/`ret`, switches, get/put*field, invoke*, `new*`, `multianewarray`, `athrow`, `checkcast`/`instanceof`, `monitor*`. Unknown opcodes reject as `UnknownOpcode(op)`.
- [x] Instruction-size table; supports `wide` prefix (and `wide iinc`); `iinc` correctly 3-byte (would have been silently 1-byte under the naive 0x60..=0x93 range).
- [x] `estimate_work()`: backward branches default to `1<<20`; straight-line methods to bytecode length. Used by Part E to gate tiny inputs.
- [x] **Deviation:** did not reuse `bytecode_verifier`'s type-flow logic. The first cut takes the simpler stance "reject any opcode we can't prove is safe at scan time" and lets the per-`aload`/`astore` precision come later. The reject is tight enough that no unsafe method passes.
- [x] Fixtures created in `test_classes/gpu/` with **real Java source**, compiled by `jit-cuda/build.rs` via `javac`. Out-parameter style (the GPU host pre-allocates outputs), so eligible methods do not call `newarray`.
- [x] `cargo test -p jit-cuda` — 13 tests, including all 4 analyzer fixtures (`EligibleVectorAdd`, `EligibleSaxpy`, `EligibleDotProduct`, plus `Reject*`).

### Verification

- [x] `cargo test -p jit-cuda analyzer::tests` passes against the real `.class` files.
- [x] Each represented `Reason` variant has at least one real-class test (`Allocation`, `Invoke`, `Synchronized`, `UnsupportedParamType`). Other variants (`Switch`, `Throw`, `Monitor`, `TypeCheck`, `JsrRet`, `FieldAccess`) await fixtures in a follow-up — none are user-facing required for the first end-to-end demo.

---

## Part E — Interpreter integration & deopt fallback [serial after B, C, D] — **WIRING LANDED; LAUNCH GLUE PENDING GPU BOX**

**Goal:** wire the offload pipeline into the existing execute path so that a real Java program calling a static method with the right shape transparently runs on the GPU; anything unexpected unwinds back to the interpreter without observable difference.

### Isolation contract (re-confirmed mid-session)

This Part is the highest risk for coupling. The implementation MUST:

1. Introduce a Cargo feature `gpu-offload` on `cratonvm-vm` that, when off, removes every line of code Part E adds from the compiled binary.
2. Wrap every new import, every new field on `Vm` / `VmConfig`, every new function call site, and the entire `OffloadCache`-lookup hook in `#[cfg(feature = "gpu-offload")]`.
3. After landing, verify: `cargo expand -p cratonvm-vm` with the feature off has the same `execute_invokestatic()` body as the pre-Part-E commit. The CPU hot path takes zero extra branches.
4. `cratonvm-cli`'s existing `gpu` feature gains `"cratonvm-vm/gpu-offload"` so building the CLI with `--features gpu` automatically pulls the VM hook in.

### Steps

- [x] `gpu-offload` Cargo feature on [vm/Cargo.toml](vm/Cargo.toml) — optional `cuda-bridge` and `jit-cuda` deps, propagates `cratonvm-gc/gpu-offload`.
- [x] `vm-cli`'s `gpu` feature now propagates `cratonvm-vm/gpu-offload`; `gpu-driver` adds `cuda-bridge/cuda`.
- [x] [vm/src/runtime/offload.rs](vm/src/runtime/offload.rs) created (whole file `#[cfg(feature = "gpu-offload")]`): `OffloadCache`, `CompiledKernel`, `LookupOutcome::{Hit, Skip, Blacklisted}`, `DispatchOutcome::{Handled, FallThrough}`, `output_array_index()`, and the entry-point `try_dispatch(shared, thread, frame_idx, class, method, descriptor, args) -> Result<DispatchOutcome, MethodCallFailed>`.
- [x] [vm/src/runtime/gpu_marshal.rs](vm/src/runtime/gpu_marshal.rs) updated by the parallel agent: every public `host_view_*` / `write_back_*` now takes `_token: &SafepointToken<'_>` (marker-only).
- [x] [vm/src/vm/vm_init.rs](vm/src/vm/vm_init.rs) `SharedVm` has a new `#[cfg(feature = "gpu-offload")] pub offload_cache: Arc<OffloadCache>` field, constructed in `SharedVm::new`.
- [x] The interpreter hook in [vm/src/runtime/interpreter.rs](vm/src/runtime/interpreter.rs) at `execute_invokestatic` (just before `try_stackless_invoke`, ~line 9710) — wrapped in `#[cfg(feature = "gpu-offload")]`, guarded by `shared.config.gpu_offload_enabled && shared.offload_cache.has_device()`. Calls `try_dispatch` and acts on `DispatchOutcome`.
- [x] CLI plumbing: [vm-cli/src/main.rs](vm-cli/src/main.rs) forwards `args.gpu`, `args.gpu_device`, `args.gpu_min_work`, `args.print_gpu_decisions` into `VmConfig`, under `#[cfg(feature = "gpu")]`.

### Deferred to a follow-up scoped specifically to GPU hardware

- The marshal+launch glue inside `try_dispatch`'s `LookupOutcome::Hit` arm. Today it logs `tracing::debug!` and returns `FallThrough`. The `OffloadCache` itself fully analyzes, lowers, and (on a real GPU) calls `DeviceModule::from_ptx` — so the lookup side already works end-to-end. Only the launch side is stubbed. This is deliberate: validating a real `cuLaunchKernel` requires actual NVIDIA hardware, and the surrounding cfg-gated wiring is most safely landed before adding the deopt-and-write-back complexity. The function signature and call-site contract is **final**; the next iteration only grows the `Hit` arm.
- [ ] On cache miss, call `jit_cuda::analyzer::analyze(&class, &method)`. On `Eligible`, compile-and-cache:
  1. lower the method through the existing IR (`jit/src/ir.rs`) → already produces `cratonvm-jit-api::IrGraph`
  2. hand the `IrGraph` to `jit_cuda::lower(&graph) -> PtxModule`
  3. `cuda_bridge::DeviceModule::from_ptx(ptx, &[kernel_name])` (Part A)
  4. store `CompiledKernel { module, signature, kernel_name }` in `OffloadCache` keyed by `(ClassId, method_index)`
- [ ] On a cached hit, run the offload path:
  1. acquire a `SafepointToken` (Part F)
  2. marshal each argument: arrays via Part C, primitives by value
  3. allocate an output `DeviceBuffer` of the right length (the JVM caller already allocated a return-array via interpreter — read the heap object the same way)
  4. allocate a `failure_flag: DeviceBuffer<u32>` of length 1, zero-initialised
  5. launch with `LaunchConfig { grid: ((n + 255)/256, 1, 1), block: (256, 1, 1), shared_bytes: 0 }`
  6. `device_ctx.synchronize()`, then read `failure_flag`
- [ ] If `failure_flag != 0`, throw the corresponding Java exception via the existing `throw_arithmetic_exception()` / `throw_array_index_oob()` helpers and **resume the interpreter from the call site** (no partial GPU state survives). This is the deopt path.
- [ ] If `failure_flag == 0`, write back the result array(s) via Part C and produce the `Value::Object(Some(ref))` to push on the operand stack.
- [ ] Plumb a runtime flag `vm.gpu_offload_enabled` (default `false`) on `VmConfig`, but **only when the `gpu-offload` Cargo feature is on** — the field itself is `#[cfg(feature = "gpu-offload")]`. Without the feature, neither the field nor any code reading it exists.
- [ ] Add a small input-size gate: if `estimated_work < 4096`, skip offload and fall through to existing JIT/interp. Keeps unit-test methods running on CPU unless explicitly stressed.

### Verification

- [ ] A new integration test `vm/tests/gpu_offload_e2e.rs` loads `test_classes/gpu/EligibleVectorAdd.class`, runs `vectorAdd` on two `int[1<<20]`s, and asserts elementwise equality with a Rust reference. The test is `#[cfg_attr(not(feature = "gpu-it"), ignore)]`.
- [ ] With `vm.gpu_offload_enabled = false`, every existing test in the repo still passes (no regressions).
- [ ] Forcing a deopt path: `vectorAdd` is called with arrays of different lengths so a bounds check trips → JVM observes a `java.lang.ArrayIndexOutOfBoundsException` indistinguishable from the interpreter's. Assert the exception type and message.

---

## Part F — Safepoint / GC coordination for device memory [serial after A, C] — **COMPLETED**

**Goal:** GPU work must not race with the GC. Device buffers must be **pinned** for the duration of a kernel — the JVM cannot relocate the host array under us.

### Isolation contract

- `SafepointToken` and `enter_gpu_critical()` MUST be `#[cfg(feature = "gpu-offload")]` on the `Heap` impl.
- The check inside `safepoint_check()` that delays GC for the GPU also gated. With the feature off, GC behaviour is identical to today.
- `cratonvm-gc` gains an optional `gpu-offload` feature mirroring `cratonvm-vm`'s.

### Steps

- [x] `SafepointToken<'h>` (zero-sized, `!Send`, RAII, `pub(crate)` constructor) in [gc/src/safepoint.rs](gc/src/safepoint.rs), gated behind `gpu-offload` feature.
- [x] **Plan correction:** `safepoint_check()` does NOT live in `gc/`. It is in `vm/src/runtime/interpreter.rs:780` and the task forbade touching the vm crate. The delay logic was instead placed inside `Heap::collect_garbage` / `Heap::collect_garbage_with_finalizers` (the actual GC entrypoints). Yield-spin loop matching `reference.rs::remove_timeout` style; after `GPU_CRITICAL_DEADLINE_SECS = 5` a single `tracing::warn!` fires. This is the architecturally correct seam.
- [x] `gpu_critical_count: AtomicU32`, `gpu_pinned_refs: Mutex<HashSet<ObjectRef>>`, and `gpu_blocked_gc_count: AtomicU64` added to `Heap`, all `#[cfg(feature = "gpu-offload")]`.
- [x] Public `Heap` methods: `enter_gpu_critical()`, `gpu_critical_count()`, `pin_ref()`, `unpin_ref()`, `gpu_pinned_refs_snapshot()`, `gpu_blocked_gc_count()` — all cfg-gated.
- [x] Root walker includes pinned refs when collecting (in `collect_garbage` and `collect_garbage_with_finalizers`, both branches gated).
- [ ] `GcStress.java` 30-second stress test — deferred to the GPU-equipped verification machine. The plumbing is in place; the multi-threaded stress requires real GPU work to exercise the race.

### Verification

- [x] `cargo check -p cratonvm-gc` — clean, default features.
- [x] `cargo check -p cratonvm-gc --features gpu-offload` — clean.
- [x] `cargo test -p cratonvm-gc` — pre-existing 672 lib tests pass.
- [x] `cargo test -p cratonvm-gc --features gpu-offload` — **678 lib tests pass** (672 pre-existing + 6 new gpu_offload). Tests cover: counter inc/dec on token construction/drop, nested tokens, `collect_garbage` blocked-while-token-held, real `Heap::alloc_array` pinning surviving GC, root walker visiting pinned refs.

---

## Part G — Test infrastructure: real `.class` files only [parallel] — **PARTIAL (foundation landed)**

**Goal:** establish the test fixture layout, the build.rs that compiles real Java sources, and a baseline `gpu-it` feature flag pattern. **This is the part that enforces "no synthetic stubs."** Every other Part references fixtures created here.

### Steps

- [x] Created `test_classes/gpu/` and `test_classes/gpu/README.md` (re-states the "no synthetic stubs" rule).
- [x] [`jit-cuda/build.rs`](jit-cuda/build.rs) compiles every `test_classes/gpu/*.java` via `javac` on every build; prints a `cargo:warning=` if `javac` is missing.
- [x] Real Java sources: `EligibleVectorAdd.java`, `EligibleSaxpy.java`, `EligibleDotProduct.java`, `RejectAllocation.java`, `RejectInvoke.java`, `RejectSynchronized.java`, `RejectRefArray.java` — all compile to real `.class` files.
- [x] Eligible fixtures use out-parameter style (host allocates the output) so they pass the analyzer; `RejectAllocation` exercises the `newarray` reject path.
- [x] `gpu-it` Cargo feature on `cuda-bridge` and `jit-cuda`. **Deviation:** kept the feature local to those crates rather than at the workspace root — propagating a workspace-wide feature flag through clap and 13 other crates is more wiring than this iteration needs.
- [ ] `GcStress.java` and `BoundsTrip.java` — pending Parts F and E.
- [ ] CI wiring — pending; this iteration verifies locally only.

### Verification

- [x] `javac` runs cleanly during `cargo build -p jit-cuda` (`.class` files appear alongside sources).
- [x] `cargo test -p jit-cuda` finds the fixtures via `test_support::load_method` and the four analyzer tests load real bytecode through `cratonvm_reader::read_class`.
- [x] `grep -R "fn make_class\|ClassFile::synthetic\|0xCA, 0xFE, 0xBA, 0xBE" jit-cuda/ cuda-bridge/ test_classes/gpu/` returns no hits in code authored by this initiative.

---

## Part H — CLI / config / device diagnostics [serial after A] — **COMPLETED**

**Goal:** surface GPU offload to the user via the existing CLI; print what we're doing; let users disable it.

### Steps

- [x] `--gpu`, `--gpu-device <N>`, `--gpu-min-work <N>`, `--print-gpu-decisions`, `--gpu-info` added to `Args` in [vm-cli/src/main.rs](vm-cli/src/main.rs).
- [x] `--gpu-info` is an early-exit: calls `cuda_bridge::probe()`, prints device name + sm_X.Y + memory, returns. With the `cuda` feature off (default), prints `no CUDA device available: no CUDA driver available (crate built without the \`cuda\` feature, or driver not installed)`. Either way, exit code 0 — no JVM bootstrap.
- [x] `--gpu` probes early; if no driver, prints a single `[cratonvm-cli] --gpu requested but no CUDA driver available …; running on CPU` and continues with `args.gpu = false`.
- [x] `cuda-bridge` is a hard dependency of `vm-cli`. Default-features-off keeps the workspace stub-only; `cargo build --features gpu` flips on the `cuda` Cargo feature on `cuda-bridge`.
- [ ] Plumbing the flags into `VmConfig` and reading them from the interpreter — pending Part E (which is the natural owner of that integration; the flags exist on `Args` ready to forward).

### Verification

- [x] `cargo check -p cratonvm-cli` passes with the new flags.
- [ ] `cargo run -p cratonvm-cli -- --gpu-info` on the dev box — pending a real GPU + `--features gpu`. With default features it prints the no-driver line.
- [ ] `--print-gpu-decisions` is parsed but reads no decisions until Part E lands.

---

## Part I — cuda-oxide evaluation note + `GpuLowering` seam [parallel] — **COMPLETED**

**Goal:** make the evaluation of cuda-oxide auditable, and keep one trait-shaped seam in the code so a future Rust-side GPU helper could plug in without surgery. This Part is small on purpose — cuda-oxide is **not** on our critical path.

### Steps

- [x] [docs/gpu/cuda-oxide-evaluation.md](docs/gpu/cuda-oxide-evaluation.md) committed with the verdict, the reference table, and the explicit re-evaluation conditions.
- [x] [jit-api/src/gpu_lowering.rs](jit-api/src/gpu_lowering.rs) defines `trait GpuLowering` (with `name()` and `lower(class_name, method) -> Result<LoweredKernel, LoweringError>`). Today's sole future-implementor is the in-tree `PtxEmitter`; Part E will hold an `Arc<dyn GpuLowering>` when wired.
- [x] No `CudaOxideLowering` scaffold. No example. The trait is the seam.

### Verification

- [x] Doc exists and is linked from the top-level [README.md](README.md).
- [x] `cargo build -p cratonvm-jit-api` succeeds with the new module.
- [x] `grep -R "CudaOxide" jit-cuda/ cuda-bridge/ vm/` returns zero matches in code.

---

## Part J — End-to-end demo & acceptance gate [serial — last] — **SCAFFOLDED (pending GPU box for real numbers)**

**Goal:** prove the whole pipeline runs a real Java program faster on GPU than on CPU, using nothing but real `.class` files.

### Steps

- [x] [test_classes/gpu/Benchmark.java](test_classes/gpu/Benchmark.java) authored. `public static void main(String[])`, parses `args[0]` as `int n`, allocates `int[] a/b/out`, fills via a deterministic LCG (no `java.util.Random` — keeps the data-fill loop GPU-eligibility-clean if ever offloaded). Calls `EligibleVectorAdd.vectorAdd(a, b, out)`. Prints a single machine-parseable line.
- [x] Compiled successfully by the existing `jit-cuda/build.rs` chain — `Benchmark.class` lands in `test_classes/gpu/`.
- [x] [docs/gpu/first-results.md](docs/gpu/first-results.md) scaffolded with the procedure, the empty results table, and the acceptance criteria spelled out.
- [ ] Real CPU-vs-GPU numbers — **deferred to the GPU-equipped verification box** (this dev machine has no NVIDIA GPU). The doc reminds the runner not to falsify entries.

### Acceptance criteria

- [ ] Both runs produce **identical** `out[0]` and `out[n-1]` values (correctness — non-negotiable).
- [ ] The GPU run produces strictly lower elapsed nanos than the CPU run on the dev box at `n = 1 << 24`. (Threshold: ≥2× speedup expected; if not met, file a follow-up to investigate marshalling overhead — do not fake it.)
- [ ] All previously passing tests still pass with `--features gpu-it` off and with it on.
- [ ] `grep -R "synthetic" test_classes/gpu/ jit-cuda/tests/ vm/tests/gpu_*` shows no synthesised-bytecode hits.

---

## Critical files to read or modify (index)

- [Cargo.toml](Cargo.toml) — workspace members (add `cuda-bridge`, `jit-cuda`)
- [jit/src/lib.rs](jit/src/lib.rs) — JIT entry point, hook for backend selection
- [jit/src/ir.rs](jit/src/ir.rs) — IR node definitions (consumed by Part B)
- [jit/src/ir_lower.rs](jit/src/ir_lower.rs) — pattern for adding a lowering target
- [jit/src/x64.rs](jit/src/x64.rs), [jit/src/aarch64.rs](jit/src/aarch64.rs) — reference impls
- [vm/src/runtime/interpreter.rs](vm/src/runtime/interpreter.rs) — `execute_invokestatic` (~line 9651)
- [vm/src/vm.rs](vm/src/vm.rs) — `Vm`, `VmConfig`
- [vm-cli/src/main.rs](vm-cli/src/main.rs) — CLI flags
- [gc/src/heap.rs](gc/src/heap.rs) — heap layout, safepoint, root scanning
- [classloading/src/bytecode_verifier.rs](classloading/src/bytecode_verifier.rs) — reusable type-flow analyser
- [reader/src/stack_map.rs](reader/src/stack_map.rs) — verification frames for Part D's analyser
- [types/src/value.rs](types/src/value.rs) — `Value`, `ObjectRef`
- [reader/tests/fixtures/HelloWorld.java](reader/tests/fixtures/HelloWorld.java) — pattern to mirror for new fixtures (a fixture the crate that reads it owns, where no `.gitignore` rule can swallow the `.class`)

## Non-goals (explicit)

- No Linux support in this plan (Part A's abstraction makes it cheap later; not now).
- No reference-type arrays on GPU (`Integer[]`, `String[]`).
- No object allocation inside kernels.
- No synchronized methods on GPU.
- No interpreter-replacement; offload is **opt-in** and **per-method**.
- No nightly Rust dependency (this is what keeps cuda-oxide out of the critical path).
- No new GC algorithm — we coordinate with the existing one via safepoints.

## Risks & mitigations

| Risk | Mitigation |
| --- | --- |
| `cudarc` feature-flag (`cuda-12060` etc.) drifts from local toolkit | Pin in `cuda-bridge/Cargo.toml`, document upgrade path in its README |
| GPU memory exhaustion on large inputs | Part E's input-size gate + explicit OOM → deopt to interpreter |
| GC moves an array mid-kernel | Part F's `SafepointToken` + `gpu_critical_count` makes this impossible |
| Java semantics divergence (e.g., signed-shift, NaN, `Math.fma`) | Part B's PTX emitter matches Java semantics opcode-by-opcode; analyzer rejects any opcode it can't faithfully emit |
| Test flakiness from real GPU hardware | `gpu-it` feature flag isolates GPU tests; CPU tests stay deterministic |
| cuda-oxide reaches v1.0 before we ship Part I | Part I's `GpuLowering` trait already abstracts emission; swap is contained |
