# Evaluation: [cuda-oxide](https://nvlabs.github.io/cuda-oxide/) and CratonVM

**Status:** re-evaluated 2026-09-04. The *compiler* is still not adopted
and still should not be. Its **host runtime** is now an opt-in second
driver backend: `--features cuda-oxide` on `cratonvm-cuda-bridge`,
`--features gpu-driver-oxide` on `cratonvm-cli`.

This document exists so future readers don't re-litigate the question.
The 2026-09-03 revision answered a flat "no" to a question that turned
out to have two halves, and got one of them wrong. What follows keeps
the half that was right and records the half that was missed.

## The correction (2026-09-04)

The previous revision said cuda-oxide "is **not** a competitor to
cudarc … They solve different problems." That is true of the
**compiler**, which is what was evaluated. It is not true of the
**project**, and the difference was never checked.

The repo ships `crates/cuda-host`, and `cuda-host` is built on
[`cuda-core`](https://crates.io/crates/cuda-core) — "Idiomatic CUDA
API", from NVlabs/cutile-rs. `cuda-core` is a host-side driver bridge:
contexts, streams, events, modules, device buffers. That is exactly
cudarc's job, from the same vendor as the compiler.

The failure mode was inferring absence from a search that could not
find it: the conclusion was drawn from the project's description
("Rust-to-CUDA compiler") without listing its crates. `cuda-host`'s
manifest names `cuda-core` on its second line.

Three further claims in that revision were stale or wrong:

| Claim (2026-09-03) | Actual (2026-09-04, measured) |
| --- | --- |
| "v0.1.0 (early 2026)" | Repo created 2026-04-22; `cuda-core` is 0.3.1, published 2026-09-04 |
| "requires a nightly toolchain" | True of `rustc-codegen-cuda` and `cuda-host`. `cuda-core` carries **no** `#![feature(...)]` and builds on **stable** |
| "ships its own device runtime … `DeviceBuffer`" | Correct, and that runtime is the useful part, not a reason to decline |

## What is still true: don't use the compiler for Java

The CratonVM problem is **Java bytecode → PTX**, and cuda-oxide's input
is Rust source. Using it on the Java path would mean synthesising Rust
per Java method and running rustc on every JIT invocation — strictly
worse than lowering bytecode → PTX in the existing pipeline, which
`jit-cuda` already does.

That reasoning is unchanged and is why the backend uses
`load_module_from_ptx_src` — cuda-core's runtime `cuModuleLoadData`
wrapper — and not `#[cuda_module]`, cuda-oxide's headline API, which
embeds artifact bundles compiled at **build** time. A kernel does not
exist until the VM has seen the method.

## How GPU offload is wired (isolation contract)

GPU offload is **opt-in** and **strictly separate from the CPU path.**
The default `cargo build` produces a CPU-only JVM that is byte-identical
to the pre-GPU codebase — no `cuda-bridge` link, no `--gpu*` CLI flags,
no GPU branches in the interpreter.

Feature levels (Cargo features on `cratonvm-cli`):

| Build invocation | What you get |
| --- | --- |
| `cargo build` | CPU JVM. No GPU code linked. No `--gpu*` flags. |
| `cargo build --features gpu` | + `cuda-bridge` stub backend, `--gpu*` flags visible. Probe → `NoDriver`. |
| `cargo build --features gpu-driver` | + real bindings to libcuda / nvcuda.dll **via cudarc**. |
| `cargo build --features gpu-driver-oxide` | + real bindings **via NVlabs `cuda-core`**. Linux only, see below. |

The last two are mutually exclusive: both supply `cuda-bridge`'s
`backend` module, and enabling both is refused with a `compile_error!`
rather than resolved by precedence.

Every GPU integration into a CPU-path crate (`cratonvm-vm`,
`cratonvm-cli`, `cratonvm-jit-api`, `cratonvm-gc`) lives behind
`#[cfg(feature = "...")]`. There is no runtime-gated-dead-code path
through the hot interpreter loop.

## What the second backend cost, and what it bought

It was made possible by writing the backend contract down first
(`cuda-bridge/src/backend_api.rs`, 2026-09-04). Before that the seam was
an unwritten convention, and the two existing backends had already
drifted — `from_ptx` took `&[&'static str]` in one and `&[&str]` in the
other, invisible because only one module compiles per build.

Adding a third backend forced two structural changes worth knowing about:

- **`gpu-driver` (internal feature).** Most `#[cfg(feature = "cuda")]`
  sites meant "is there a real device?", not "is this cudarc?". They now
  test `gpu-driver`, which both real backends imply. Only the sites that
  genuinely name cudarc types still test `cuda`.
- **`backend::drv`.** `event.rs` and `stream.rs` were written directly
  against `cudarc::driver::result::*`. They now call a small per-backend
  `drv` module (event create/record/query/sync/destroy, stream
  wait/sync/fork, host callback). This is where the crate's UAF and
  cross-stream-ordering audits live, and duplicating them per vendor was
  not acceptable.

**The backends are now tested against the same device suite.** Five
integration tests that exercise the bridge's own contract rather than
cudarc — driver version, transfers, event latching, stream ordering,
concurrent dispatch — moved from `cfg(feature = "cuda")` to
`cfg(feature = "gpu-driver")` and run against both.

That immediately paid for itself. `concurrent_dispatch_it`'s
`one_context_many_threads` **failed** on the first oxide build with
`got 0, want 4005` — the same defect class it was originally written to
catch in cudarc. Two real ordering bugs, both mine:

1. `to_host` enqueued `cuStreamWaitEvent` on one stream and then issued a
   *synchronous* `cuMemcpyDtoH_v2`, which runs against the NULL stream. A
   wait on a stream the copy never uses orders nothing.
2. `zeros` used `cuMemsetD8_v2` and the backend declared allocation
   "synchronous", making `record_alloc_event` a no-op. `cuMemsetD8` is
   asynchronous w.r.t. the host for device memory, and the NULL stream's
   implicit synchronisation reaches only *blocking* streams — while every
   compute stream here comes from `fork`, i.e. `CU_STREAM_NON_BLOCKING`.
   The zeroing could land after a kernel's stores and wipe them.

Both are fixed; the suite is 10/10 on the oxide backend and 12/12 on
cudarc (the extra two are the cudarc-only alloc-pool and graph-capture
suites), and the concurrency test is 6/6 across repeats.

## v1 limitations of the oxide backend

Honest gaps, not hidden ones:

- **No CUDA graph capture.** `graph.rs` stays cudarc-only; `cuda-core`
  has no graph module. An oxide build has no capture/replay, which is a
  throughput optimisation, not a correctness feature.
- **No allocation pool.** `backend_cuda::AllocPool` has no twin, so
  `set_retire_to_pool` is a no-op and allocations are freed at drop.
  Measured 2026-09-04 and it costs nothing detectable -- see the
  performance section below.
  The exit census still counts every allocation as a pool MISS
  (`cuMemAlloc=N pooled=0`). Leaving it uncounted would have printed
  `cuMemAlloc=0` on a run that allocated heavily -- a zero from an
  instrument that cannot fire, which reads as "no allocations" rather
  than "no pool".
- **Occupancy** uses the raw `cuOccupancyMaxPotentialBlockSize` symbol
  through `cuda_core::sys`; cuda-core wraps the *cluster* occupancy
  queries but not this one.
- **Windows does not build.** `cuda-core` 0.3.1 has an upstream enum
  signedness bug on MSVC — see
  `docs/known-issues/gpu/cuda-core-msvc-enum-signedness-20260904.md`,
  which includes the 13-edit fix and the evidence that it works on
  sm_75 once applied.

## Performance: measured, and no difference resolved

Both backends were A/B'd on this box (Windows 11, CUDA 13.3, RTX 2060)
with `bench-gpu/backend-ab.sh`.

| Bench | rounds | oxide vs cudarc | control (cudarc vs itself) |
| --- | ---: | --- | --- |
| `GpuTransferFloor` 2^24 | 8 | +3.1% | +1.7%, 17% spread |
| `GpuAllocChurn` 2^20 | 21 | **+0.4%, 95% CI [-4.4%, +5.1%]** | +1.8%, CI [-1.9%, +5.5%] |

Both confidence intervals include zero -- as the control's must, which is
what says the method is calibrated rather than merely quiet. **The two
backends are equivalent to within about +/-5% here.** Checksums are
bit-identical across backends on every bench, and against HotSpot on
`ci-gate.sh` (4/4 both) and `runtime-stress.sh` (8/8 both).

Three traps this measurement walked into, all caught by the control arm,
and all worth knowing before anyone re-runs it:

1. **A vacuous instrument.** The first attempt used `GpuWarm`, whose
   `warm_ms` is an INTEGER and read 1-2 ms at these sizes. Millisecond
   quantization was the entire signal: the control arm and the real arm
   both came out at exactly `2.000`. `GpuTransferFloor` and
   `GpuAllocChurn` report `best_ms` as a double.

2. **A false positive pointing the way the mechanism predicts.** cudarc
   pools device allocations and this backend does not, so `GpuAllocChurn`
   was written to force an allocation per call. A single unpaired run
   showed oxide **34% slower** -- exactly the predicted direction. Paired
   against a rotated control it came back +0.4%. The mechanism is real;
   the effect is not, at this shape and size. Had the run stopped at the
   smoke test it would have "confirmed" the alloc pool.

3. **An order effect read as drift.** At 8 rounds each successive slot in
   a round looked slower (1.338 / 1.381 / 1.402 ms). At 21 rounds -- seven
   complete rotations of the three-arm order, so every arm spends equal
   time in every slot -- it vanished (1.353 / 1.376 / 1.354 ms).

What this does NOT cover: CUDA graph capture (cudarc-only, so there is
nothing to compare), and any difference smaller than roughly 5%, which
this shared box cannot resolve.

## The seam we kept anyway

The `GpuLowering` trait in `jit-api` still has no in-workspace
implementor, and we still did **not** create a `CudaOxideLowering`
skeleton — that would be the synthetic stub the wider GPU plan forbids.
Nothing above changes that: this work adopted cuda-oxide's *host
runtime*, not its compiler.

## When to re-evaluate the compiler

Unchanged from the previous revision. Reopen when **all** hold:

1. cuda-oxide ships a stability statement covering the surface we'd use.
2. It compiles on stable Rust.
3. There is a concrete CratonVM feature needing a Rust-authored
   device-side helper — e.g. a parallel-GC mark routine that genuinely
   benefits from being written in Rust rather than emitted
   opcode-by-opcode from our own backend.

## Quick reference

| | **cudarc** (default) | **cuda-core** (opt-in) | **cuda-oxide compiler** (not used) |
| --- | --- | --- | --- |
| What it is | Safe wrapper over the CUDA Driver API | Safe wrapper over the CUDA Driver API | rustc backend lowering Rust → PTX |
| Vendor | community (coreylowman) | NVIDIA (NVlabs/cutile-rs) | NVIDIA (NVlabs/cuda-oxide) |
| Rust toolchain | stable | stable | nightly |
| Input it consumes | PTX text you produce | PTX text you produce | Rust source via MIR |
| Solves Java → PTX? | No | No | No |
| Graphs / alloc pool | yes / yes | no / no | n/a |
| Platforms here | Linux + Windows | Linux (Windows: upstream bug) | n/a |

## Pointers

- The backend: `cuda-bridge/src/backend_oxide.rs`
- The contract: `cuda-bridge/src/backend_api.rs`
- Windows bug: `docs/known-issues/gpu/cuda-core-msvc-enum-signedness-20260904.md`
- Project page: <https://nvlabs.github.io/cuda-oxide/>
- `cuda-core`: <https://crates.io/crates/cuda-core> · <https://github.com/nvlabs/cutile-rs>
- cudarc on crates.io: <https://crates.io/crates/cudarc>
- The GpuLowering trait: `jit-api/src/gpu_lowering.rs`
- Our PTX emitter: `jit-cuda/src/emitter.rs`
