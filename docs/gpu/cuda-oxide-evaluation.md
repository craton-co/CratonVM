# Evaluation: [cuda-oxide](https://nvlabs.github.io/cuda-oxide/) and CratonVM

**Status:** evaluated; **not adopted** in the current GPU-offload work.

This document exists so future readers don't re-litigate the question.
If you came here asking "should we be using cuda-oxide?" — the short
answer is **no, not today**. The long answer follows.

## How GPU offload is wired (isolation contract)

GPU offload is **opt-in** and **strictly separate from the CPU path.**
The default `cargo build` produces a CPU-only JVM that is byte-identical
to the pre-GPU codebase — no `cuda-bridge` link, no `--gpu*` CLI flags,
no GPU branches in the interpreter.

Three feature levels (Cargo features on `rustjvm-cli`):

| Build invocation                                  | What you get                                                                 |
| ------------------------------------------------- | ---------------------------------------------------------------------------- |
| `cargo build`                                     | CPU JVM. No GPU code linked. No `--gpu*` flags.                              |
| `cargo build --features gpu`                      | + `cuda-bridge` stub backend, `--gpu*` flags visible. Probe → `NoDriver`.    |
| `cargo build --features gpu-driver`               | + real cudarc bindings to libcuda / nvcuda.dll.                              |

Every GPU integration into a CPU-path crate (`rustjvm-vm`,
`rustjvm-cli`, `rustjvm-jit-api`, `rustjvm-gc`) lives behind
`#[cfg(feature = "...")]`. There is no runtime-gated-dead-code path
through the hot interpreter loop.

## What cuda-oxide is

cuda-oxide is an experimental Rust-to-PTX compiler. It hooks into
`rustc` as a custom codegen backend, lifts the program's MIR through a
custom `pliron` IR (MLIR-style), and emits NVIDIA PTX. As of v0.1.0
(early 2026) it is alpha, requires a nightly toolchain, and ships its
own device runtime (`DeviceBuffer`, `DisjointSlice`, async
`DeviceOperation` graphs).

It is **not** a competitor to `cudarc`. cudarc loads PTX onto the
device and launches it. cuda-oxide produces PTX from Rust source. They
solve different problems.

## Why we are not adopting it

The CratonVM problem is **Java bytecode → PTX**. Neither cudarc nor
cuda-oxide solves that directly.

- cuda-oxide's input is Rust source. To use it on the Java path we
  would have to synthesise Rust source per Java method, parse it
  through rustc on every JIT invocation, and have it lower through
  `pliron` to PTX. That pipeline is strictly worse than lowering
  bytecode → PTX directly in our existing IR pipeline.
- cuda-oxide pins us to nightly Rust. The rest of the workspace
  targets stable. The plan to keep cudarc on stable is deliberate.
- cuda-oxide is v0.1.0. "Expect bugs, incomplete features, and API
  breakage" — quoted from its own README. The cost of taking that
  dependency on the critical path is high; the benefit is zero.

## The seam we kept anyway

We left one piece of optionality: the `GpuLowering` trait in
`jit-api`. The only implementor today is `jit_cuda::PtxEmitter`. The
trait exists so that **if** in the future we want to compile
Rust-authored device-side helpers (parallel GC mark, atomic helpers,
math intrinsics) into PTX modules and link them alongside our own
emitted kernels, we can plug a second implementor in without touching
the interpreter integration.

We did **not** create a `CudaOxideLowering` skeleton. That would be
the kind of synthetic stub the wider GPU plan explicitly forbids. An
impl arrives if and when there is a concrete need.

## When to re-evaluate

Reopen this document and rerun the evaluation when **all** of these
hold:

1. cuda-oxide ships v0.5+ with a stability statement that covers the
   surface we'd actually use (codegen, device runtime).
2. cuda-oxide compiles on stable Rust (no nightly).
3. We have a concrete CratonVM feature in flight that needs a
   Rust-authored device-side helper — e.g. a parallel-GC mark routine
   that genuinely benefits from being written in Rust rather than
   emitted opcode-by-opcode from our own backend.

Until all three are true: don't.

## Quick reference

|                      | **cudarc** (we use)                                                                  | **cuda-oxide** (we don't)                                                          |
| -------------------- | ------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------- |
| What it is           | Safe Rust wrapper over CUDA Driver API + NVRTC + cuBLAS                              | rustc codegen backend that lowers Rust → PTX in-process                            |
| Maturity             | Stable; used in burn / candle / dfdx                                                 | v0.1.0 alpha, "expect bugs, incomplete features, API breakage"                     |
| Rust toolchain       | Stable                                                                               | Nightly                                                                            |
| Input it consumes    | PTX text **you produce**, or live Rust via NVRTC                                     | Rust source via MIR                                                                |
| Solves Java → PTX?   | No                                                                                   | No                                                                                 |
| Useful to us for     | Device discovery, allocation, memcpy, kernel launch                                  | Nothing on the critical path. Possibly future Rust-side GPU helpers.               |
| Risk of taking dep   | Low                                                                                  | High — pins us to nightly, breaks on rustc churn                                   |

## Pointers

- Project page: <https://nvlabs.github.io/cuda-oxide/>
- cudarc on crates.io: <https://crates.io/crates/cudarc>
- The GpuLowering trait: `jit-api/src/gpu_lowering.rs`
- Our PTX emitter: `jit-cuda/src/emitter.rs`
