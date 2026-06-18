# cuda-oxide (a.k.a. "nvidia-oxide") — impact assessment for CratonVM

**Audience:** CratonVM maintainers deciding whether to spend engineering
time on cuda-oxide.
**Status:** research note, no code touched.
**Date of research:** 2026-05-19. cuda-oxide v0.1.0 (released 2026-05-07).

> Naming up front. The user-facing name being thrown around as
> "nvidia-oxide" / "NVIDIA Oxide" / "Oxidize NVIDIA" is in every case
> the project officially named **cuda-oxide**, published by **NVlabs**
> (NVIDIA Research) on GitHub at `NVlabs/cuda-oxide`. There is no
> separate project called "nvidia-oxide". I treat the two names as
> synonyms throughout this report.

A pre-existing evaluation already lives at
`docs/gpu/cuda-oxide-evaluation.md`. This report supersedes nothing —
it expands the analysis with information from the v0.1.0 release and
maps the technology onto CratonVM's actual crate layout. The earlier
doc's conclusion ("not adopted, not today") still stands; this report
explains why with more granularity and re-checks the gates for
re-evaluation.

---

## 1. What is cuda-oxide?

cuda-oxide is **a custom `rustc` codegen backend that compiles Rust
source down to NVIDIA PTX**, paired with a small device-side runtime
and a `cargo oxide` driver subcommand. From the project README:

> "cuda-oxide is an experimental Rust-to-CUDA compiler that lets you
> write (SIMT) GPU kernels in safe(ish), idiomatic Rust. It compiles
> standard Rust code directly to PTX — no DSLs, no foreign language
> bindings, just Rust."

Concretely the pipeline is:

1. User writes one `.rs` file containing both host code and functions
   marked `#[kernel]`.
2. `cargo oxide build` invokes `rustc` with
   `-Z codegen-backend=librustc_codegen_cuda.so`.
3. The backend sees Rust MIR, lifts the kernel functions through a
   custom IR called **pliron** (a Rust-native MLIR-style IR), and
   emits PTX text.
4. Non-`#[kernel]` host code is delegated back to the standard LLVM
   backend and compiled normally.
5. A runtime crate exposes `DeviceBuffer`, `DisjointSlice`, and an
   async `DeviceOperation` graph for stream-pool scheduling.

It is **not** a wrapper around the CUDA Driver API. It is **not** a
launcher. It is a *compiler* — the layer that produces the PTX that
`cudarc` (or any other launcher) then loads onto the device. The
ecosystem doc on the project site makes this distinction explicit:
cuda-oxide is positioned as bringing "CUDA into Rust" (kernel
authoring), as opposed to cudarc which brings "Rust to CUDA" (driver
binding).

Owner: **NVlabs / NVIDIA Research**. License: **Apache 2.0** for the
compiler crates; the `cuda-bindings` crate ships under the **NVIDIA
Software License**. Repo: <https://github.com/NVlabs/cuda-oxide>.

## 2. Current state

v0.1.0 was tagged on **2026-05-07**, twelve days before this report.
The README leads with a warning: "expect bugs, incomplete features,
and API breakage as we work to improve it."

Concrete signals of maturity:

- **Toolchain pin.** `rust-toolchain.toml` pins
  `nightly-2026-04-03` and requires the `rust-src` and `rustc-dev`
  components. The backend uses unstable `rustc` internals, so this is
  not a "we just like nightly" choice — it is a hard requirement
  driven by the codegen-backend ABI.
- **External deps.** Requires **LLVM 21+** with the NVPTX backend,
  Clang 21+ headers, and the **CUDA 12.x** toolkit. The README
  explicitly says LLVM 20 cannot lower the TMA / tcgen05 / WGMMA
  intrinsics they emit.
- **OS support.** The README only claims Ubuntu 24.04 / Linux. There
  is no Windows-host build documented, no macOS, no aarch64.
- **Examples.** 46 example kernels ship, including a GEMM reported at
  868 TFLOPS on a B200 — i.e. they are aiming squarely at modern
  data-centre silicon, not consumer Ada.
- **Production users.** None disclosed. The Phoronix / MarkTechPost /
  BigGo coverage and the HN front-page thread (item 48096692) are all
  reactions to the v0.1.0 announcement, not deployments.

Translation: this is a *real* and *officially blessed* project — not
vapor — but it is twelve-days-old alpha, on nightly, Linux-only, with
no production track record.

## 3. Layer overlap with CratonVM

CratonVM's GPU stack (per `docs/gpu/COMPARISON.md` and the source) has
three layers that could in principle be touched:

| Layer | File / crate | Today | Could cuda-oxide replace it? |
|---|---|---|---|
| Driver bridge | `cuda-bridge/src/backend_cuda.rs` (cudarc 0.13.9 + CUDA 12.6) | Loads PTX, allocs, memcpys, launches kernels | **No.** cuda-oxide does not bind the Driver API. Its `DeviceBuffer` is a *consumer* of the same Driver API and would still need a cudarc-style binding underneath. |
| PTX emitter | `jit-cuda/src/lowering.rs` + `lowering/emit.rs` | Walks Java bytecode, simulates the JVM operand stack with PTX virtual registers, emits PTX text | **No.** The input is Java bytecode, not Rust source. cuda-oxide cannot consume bytecode. To slot it in we would have to *transpile bytecode to Rust source* first, which is strictly more work than going to PTX directly. |
| Offload orchestration | `vm/src/runtime/offload.rs` (gated on `gpu-offload`) | Caches `(ClassId, method_idx) -> CompiledKernel`, marshals args, launches | **No.** This is JVM-side bookkeeping. cuda-oxide has no concept of classes, methods, or `--gpu` CLI flags. |
| **Rust-authored device helpers** (does not yet exist) | n/a | n/a | **Yes, in principle.** If we ever want to ship a parallel-mark GC kernel, an atomic-helper library, a math intrinsic block, or any *hand-written* device-side Rust, cuda-oxide is the only tool that lets us author it in safe-ish Rust and statically link the resulting PTX module alongside JIT-emitted kernels via the existing `GpuLowering` seam in `jit-api`. |

The pre-existing `cuda-oxide-evaluation.md` flags exactly the same
boundary: "cudarc loads PTX onto the device and launches it.
cuda-oxide produces PTX from Rust source. They solve different
problems." Nothing in v0.1.0 changes that — if anything, the explicit
"CUDA into Rust" framing on the ecosystem page confirms it.

Where cuda-oxide *does* offer something CratonVM lacks today: kernel-
side memory safety (bounds-checked slices via `DeviceBuffer`),
strongly-typed device intrinsics, and an async `DeviceOperation` graph
abstraction. We hand-emit those things ourselves in PTX text and
enforce safety with bytecode-level analysis. That is currently fine
for the canonical-counted-loop subset; it would *not* be fine if we
ever expand to hand-written device helpers.

## 4. Concrete migration sketch

Two scenarios are worth thinking through. The first is the wholesale
replacement people sometimes ask about; the second is the only
realistic one.

**Scenario A — replace cudarc with cuda-oxide in `cuda-bridge`.**
Not possible. cuda-oxide does not expose a Driver API binding. The
sketch is empty. (It does ship a `cuda-bindings` crate, but that is a
raw bindgen layer under the NVIDIA Software License, not a cudarc
peer.)

**Scenario B — add a `CudaOxideLowering` impl of `GpuLowering`
alongside `jit_cuda::PtxEmitter`, used only for Rust-authored device
helpers (GC mark, math intrinsics, future hand-written shaders).**
Plausible if and only if the three gates in §7 close.

If we did it:

- `jit-api/src/gpu_lowering.rs` — add no new trait methods; the
  existing `GpuLowering` seam was designed for exactly this. ~0 LoC.
- `jit-cuda/src/oxide/` (new module, **~200-400 LoC**) — thin wrapper
  that drives `cargo oxide build` on a small fixed Rust crate
  containing the helper(s) and returns the resulting PTX text. Risk:
  build-time toolchain proliferation; the host now needs nightly
  Rust + LLVM 21 + CUDA 12.x + Clang 21 just to compile the helpers.
- `cuda-bridge` — **unchanged**. The output of cuda-oxide is PTX, and
  `DeviceModuleInner::from_ptx` already takes PTX text. This is the
  key reason cuda-oxide is a drop-in for helper kernels rather than a
  re-architecture: our boundary is already at the PTX level.
- `vm/src/runtime/offload.rs` — unchanged for the JIT path. A second
  registration for the static helper module would be ~30 LoC behind a
  new feature flag.
- `Cargo.toml` workspace — new optional dev-dep on the cuda-oxide
  build crates. Should stay out of the default build entirely.
- CI — gated; CratonVM's CI does not have Linux+nightly+LLVM21
  runners today. Building cuda-oxide kernels in CI would require a
  dedicated job (probably Ubuntu 24.04 container).

Total: **plausibly under 500 LoC of Rust glue** plus a non-trivial
build-system tax. Risk concentrated in the build pipeline, not the
runtime.

## 5. Performance angle

The repo advertises a GEMM at 868 TFLOPS on B200, which is in the
ballpark of cuBLAS for that hardware and silently impressive for a
0.1.0. But:

- No published comparison vs `cudarc + nvcc` on the same problem.
- No published comparison vs hand-written PTX (which is what
  CratonVM emits today).
- No published comparison vs Rust-GPU.
- The number is for a single tuned kernel by the project's own
  authors, not for a workload mix.

For CratonVM specifically: our kernels are SIMT elementwise bodies
emitted from canonical counted loops (`EligibleVectorAdd`,
`EligibleSaxpy`, etc.). On those, the ceiling is memory bandwidth, not
codegen quality — anything sensible saturates DRAM. cuda-oxide would
not help here even hypothetically.

**Honest summary:** zero published numbers relevant to anything
CratonVM does. Treat performance as a non-argument for adoption.

## 6. Risks of adopting

- **API churn.** v0.1.0, twelve days old, README explicitly warns
  about breakage. Every rustc nightly bump risks breaking the codegen
  backend (this is the standard hazard for anything that uses
  `rustc-dev`).
- **Toolchain pin.** CratonVM is stable-Rust today and that is a
  deliberate choice. Adopting cuda-oxide pins us to a *specific*
  nightly date (currently `nightly-2026-04-03`). Every workspace
  build would need either a separate nightly toolchain just for the
  helper crate, or a global downgrade.
- **OS support.** CratonVM development happens on Windows
  (`C:\craton\...` per this worktree). cuda-oxide ships Linux-only.
  Cross-platform support would need to land upstream or be solved
  with a Linux build container.
- **License.** The compiler crates are Apache 2.0 (compatible with
  CratonVM's Apache 2.0). The `cuda-bindings` crate is **NVIDIA
  Software License**, which is *not* an OSI-approved licence and
  needs legal review before we depend on it. Note we would only need
  `cuda-bindings` if we used cuda-oxide's *runtime*; if we use only
  its compiler output (PTX text) and feed that to our own cudarc
  bridge, we never link `cuda-bindings`.
- **CUDA version.** Requires CUDA 12.x; CratonVM is on CUDA 12.6.
  Compatible today. No statement on CUDA 13.
- **Hardware bias.** The reserved intrinsics (TMA, tcgen05, WGMMA)
  target Hopper/Blackwell. Code that compiles fine for B200 may not
  PTX-cleanly lower for Ampere/Ada consumer GPUs, depending on what
  the helper actually uses.
- **Vendor lock-in.** It is an NVIDIA project that emits PTX. There
  is no portability story. CratonVM already accepts this via cudarc,
  so this is not a *new* risk — just a continuation of the existing
  one.

## 7. Verdict and recommendation

Three options, evaluated honestly:

**Adopt now.** No. There is no concrete CratonVM feature in flight
that needs a Rust-authored device helper. Adopting cuda-oxide on
spec would pay a real toolchain tax (nightly, LLVM 21, Linux-only
CI) for zero capability we can ship today.

**Track but don't adopt.** Yes — this is the recommendation. The
existing `GpuLowering` seam in `jit-api` already provides the
extension point. We do nothing now and re-evaluate when **all** of
the following are true:

1. cuda-oxide reaches v0.5+ with a documented stability statement
   covering the compiler-backend surface we'd actually depend on.
2. cuda-oxide compiles on **stable** Rust, or pins a long-lived
   nightly with an explicit support window.
3. CratonVM has a concrete in-flight feature that *needs* a
   Rust-authored device helper — the leading candidate is a
   parallel-GC mark routine. Until such a feature exists, the gate
   is closed by design.
4. Windows-host or Linux-container build story is documented.

**Ignore.** Tempting given the 0.1.0 status and the irrelevance to the
critical path, but wrong — NVIDIA-backed projects in this space tend
to mature fast, and the upside (safe Rust device helpers) is real if
and when we need them. The cost of leaving the existing
`GpuLowering` seam in place is zero; ripping it out would be
premature.

**Recommended action:** none. Keep `docs/gpu/cuda-oxide-evaluation.md`
as the canonical short answer; this report stays as the long answer.
Re-read both on the next major cuda-oxide release.

## 8. Sources

- [NVlabs/cuda-oxide README on GitHub](https://github.com/NVlabs/cuda-oxide)
- [The cuda-oxide Book — nvlabs.github.io](https://nvlabs.github.io/cuda-oxide/index.html)
- [The Rust + GPU Ecosystem (cuda-oxide ecosystem page)](https://nvlabs.github.io/cuda-oxide/appendix/ecosystem.html)
- [Phoronix — "NVIDIA Releases CUDA-Oxide 0.1 For Experimental Rust-To-CUDA Compiler"](https://www.phoronix.com/news/NVIDIA-CUDA-Oxide-0.1)
- [MarkTechPost — "NVIDIA AI Just Released cuda-oxide" (2026-05-09)](https://www.marktechpost.com/2026/05/09/nvidia-ai-just-released-cuda-oxide-an-experimental-rust-to-cuda-compiler-backend-that-compiles-simt-gpu-kernels-directly-to-ptx/)
- [BigGo Finance — "NVIDIA Releases CUDA-Oxide 0.1"](https://finance.biggo.com/news/202605100537_NVIDIA_CUDA-Oxide_0.1_Rust_compiler)
- [Hacker News discussion (item 48096692)](https://news.ycombinator.com/item?id=48096692) — front-page thread; not fetched at report time due to rate limit, included for traceability
- [cuda-oxide on crates.io](https://crates.io/crates/cuda-oxide) — crate index entry
- Pre-existing internal evaluation: `docs/gpu/cuda-oxide-evaluation.md`
- Internal references: `cuda-bridge/Cargo.toml`, `cuda-bridge/src/backend_cuda.rs`, `jit-cuda/src/lowering.rs`, `jit-cuda/src/lowering/emit.rs`, `vm/src/runtime/offload.rs`, `docs/gpu/COMPARISON.md`
