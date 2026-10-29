# The AMD HIP backend: what was chosen, what was refused, and what is gated by what

**HIP Backend.** The third backend and the first that is not
NVIDIA.

This file exists for the same reason
[`cuda-oxide-evaluation.md`](cuda-oxide-evaluation.md) does: so the next
person has the analysis rather than the conclusion, and so nobody
re-litigates a decision from a summary of it.

Two documents used to forbid this work — `ROADMAP.md` called
multi-vendor backends a non-goal, and `docs/PLATFORMS.md` said no
AMD/ROCm backend existed or was planned. Both are rewritten. The
argument they made was "multi-backend support is TornadoVM's niche",
which is a positioning statement, not a technical one; the technical
reason HIP is affordable and OpenCL still is not is in
[Why HIP and not OpenCL](#why-hip-and-not-opencl) below.

## The one thing to know

**This backend has never run on an AMD GPU.** This project has none:
the dev box and the self-hosted CI runner are both Windows 11 with an
RTX 2060. Everything below is honest about which claims are checked and
by what.

| Claim | Checked by | Runs |
|---|---|---|
| It compiles | `cargo check --features hip` | every PR |
| A missing runtime reports rather than panics | `hip_no_driver_it.rs` | every PR |
| The renderer's output does not drift | `jit-hip` goldens + coverage manifest | every PR |
| **The emitted HIP C++ compiles and computes the right answers** | `host_shim_oracle.rs`, via `g++`/MSVC | every PR |
| hiprtc compiles it, a module loads, a kernel launches | `hip_module_it.rs` | **blocked: needs an AMD GPU** |
| A Java method runs end to end on a Radeon | `tools/gpu/amd-remote/bringup.sh` | **blocked: needs an AMD GPU** |
| Performance | nothing yet | **blocked** |

The host-shim oracle is the row that makes the rest defensible. It
takes the generated C++, compiles it with an ordinary host compiler,
and runs it against a reference implementing the Java semantics — at
wavefront 32 **and** 64. It is not a substitute for hardware, and the
table says so; but it is the difference between "this looks right" and
"this computes the right numbers".

It earned that place immediately, finding three real defects in its
first two runs:

* `char* rd0, rd1, rd2;` — in C++ `*` binds to the declarator, so every
  64-bit register but the first was a single byte. The golden test had
  recorded it happily.
* predicate register `%p1` and scalar kernel parameter `p1` collide
  once PTX's sigil is gone: "redefinition of formal parameter".
* its own launch macro used `b` for the block counter, shadowing a
  driver's array of the same name.

## The binding crate

| Crate | Version | Licence | Needs to BUILD | Verdict |
|---|---|---|---|---|
| **`cubecl-hip-sys`** | `=7.14.6085001` | MIT / Apache-2.0 | nothing | chosen |
| `rocm-rs` | 0.5.2 | MIT | ROCm 6.3+ and an AMD GPU | "early development"; safe wrappers only for rocFFT |
| `hip-sys` | 0.1.2 | MIT / Apache-2.0 | ROCm | two releases in three years |

The decisive property is the middle column, and it is the whole reason
this backend is in CI and `cuda-oxide` is not.
`.github/workflows/cuda-bridge.yml` spends twenty lines explaining that
the oxide backend cannot be gated anywhere, because `cuda-bindings`
runs `bindgen` over the vendor's headers and needs a CUDA 13 toolkit
plus libclang. `cubecl-hip-sys` ships **pre-generated** bindings and
`dlopen`s `amdhip64`/`hiprtc` at first use — so `cargo check --features
hip` runs on a stock `ubuntu-latest` with no ROCm at all, exactly as
`--features cuda` does.

Pinned with `=` rather than a caret: the version number encodes a ROCm
release, not a semver compatibility promise. Same treatment as
`cudarc`'s `cuda-12060` feature.

### The loader panics, and that is a hard constraint

`cubecl_hip_sys::dynamic::load` panics when the libraries are missing
**and** when an individual symbol is missing — its own doc says so,
because the generated wrappers have no common error type to return
through.

`BackendApi::probe_device` is documented as "must not panic on a
machine with no driver", and the VM's entire fall-back-to-CPU path
rests on getting an `Err`: `OffloadCache::new` probes, and on `Err`
logs "requested but unavailable; falling back to CPU" and sets
`ctx = None`. Without a guard, a `cratonvm --gpu` built for HIP and run
on a box without ROCm would **abort the JVM at startup** instead of
running the program on the CPU — a crash on the most ordinary
configuration there is.

So every entry point in `backend_hip.rs` opens with a guard over the
crate's own cached `is_available()`, and the two genuinely optional
entry points — occupancy and the host callback — are probed once under
`catch_unwind` and degrade rather than abort. `hip_no_driver_it.rs` is
the test, it is not `#[ignore]`d, and it is the one HIP test public CI
can actually run.

## Why HIP and not OpenCL

HIP's host API is a near-1:1 renaming of the CUDA Driver API.
`hipStream_t`/`hipEvent_t`/`hipModule_t`/`hipFunction_t`/
`hipDeviceptr_t` line up one for one; `hipModuleLaunchKernel` takes the
same `void**` argument array as `cuLaunchKernel`; every one of
`DriverApi`'s twelve operations has an exact twin. So
`backend_hip.rs` is deliberately a transcription of `backend_oxide.rs`,
and the ordering discipline this crate has accumulated — which stream a
copy is issued on, which event a consumer waits for, when the device is
bound — transfers verbatim. Departing from it would be a regression,
not an adaptation.

Kernel source is **text** on both sides, so the seam
`DeviceModuleApi::from_ptx(ctx, &str, ...)` already had the right shape
and needed a rename rather than a redesign.

OpenCL has neither property. Its argument model is indexed
`clSetKernelArg` calls rather than a pointer array, so `KernelArgs`
would need reworking; its queue-and-event model is close but not the
same; and it would buy a third vendor this project has no users on.
SPIR-V is worse again — a binary format, so the renderer would be an
encoder rather than a text emitter.

## What differs from CUDA, and what it cost

**Compilation moved in-process.** PTX is handed to the CUDA driver at
module load and a bad module fails with an opaque code — `offload.rs`'s
log line reads `PTX load failed ... ({e}); blacklisting` with nothing
actionable in `{e}`. hiprtc compiles HIP C++ here, so the error carries
the compiler's own diagnostic. That is strictly better and it is used.

**There is no context object.** HIP's current device is per-thread, set
by `hipSetDevice`, so `bind_to_thread` is that call.

**Fork ordering is explicit.** cudarc's `fork_default_stream` records
on the default stream and waits internally;
`hipStreamCreateWithFlags` does not, so `new_forked_stream` does it by
hand. A stream that silently observed nothing would break the ordering
`launch.rs` assumes.

**`hipGetDeviceProperties` does not exist** in these bindings: ROCm 6
changed the struct layout and versioned the accessor to
`hipGetDevicePropertiesR0600`.

**The launch geometry is grid-then-block**, and `cubecl-hip-sys`'s own
end-to-end test passes them the other way round — surviving only
because its element count happens to work either way. Pinned here by a
unit test rather than left to an integration test no CI can run.

## The wavefront width is a correctness problem

`warpSize` is **64 on CDNA/GCN and 32 on RDNA**. It is reported
per-device, it is not in the `gfx` string, and in HIP it is a
non-constexpr built-in.

The reduction epilogue folds a warp with five `shfl.sync.down` steps
hardcoded to 32 lanes (`docs/gpu/reductions.md`). On a 64-wide
wavefront that folds lanes 0–31 and **leaves 32–63 unaccumulated**. The
kernel does not crash, does not trip the failure flag and does not
deopt — it returns a plausible number that is wrong by roughly half.

So:

* `HipCaps::warp_size` is carried from the probe;
* `jit_hip::target::wavefront_for_arch` answers from a written table
  and **refuses** an architecture it does not know, rather than
  defaulting. Both possible defaults are wrong on half the hardware and
  neither failure is visible. `jit_cuda::target` makes the same
  argument in the other direction — its tables saturate downward
  because guessing high produces a module that cannot load — and here
  there is no safe direction at all;
* the VM refuses to lower for an unknown architecture and says so,
  running on the CPU instead;
* the host-shim oracle parameterises `warpSize` so a width-dependent
  kernel can be run at both. Nothing in the rendered set reads it yet
  — the warp fold is still unstructured — so the hook is built and the
  sweep is one converted emission site away.

## Floating point

Java's semantics are the acceptance criterion, and two clang defaults
break them.

**Contraction.** clang's HIP toolchain defaults to
`-ffp-contract=fast-honor-pragmas`, which fuses `a*b + c` into a
single-rounded FMA where JLS §15.17.1/§15.18.2 require the product
rounded to `float` **before** the add. This is the identical defect
`Emitter::binop_f32` records being found and fixed on the PTX side,
where the lever was an explicit `.rn` on every arithmetic op. Here the
lever is `-ffp-contract=off` in the hiprtc option list, and the
renderer also emits `#pragma clang fp contract(off)`: the flag guards
against a caller dropping it, the pragma guards against the default
changing.

**Denormals.** The PTX this project emits carries no `.ftz` anywhere,
so the CUDA path preserves f32 denormals and so does HotSpot. AMD's
default has moved across ROCm releases, so it is pinned with
`-fno-gpu-flush-denormals-to-zero` rather than inherited.

The option list is closed and `hip_compile_options_are_ieee` asserts
both the required entries and the forbidden ones — `-ffast-math`,
`-funsafe-math-optimizations`, `-ffinite-math-only`,
`-munsafe-fp-atomics`. A closed list with a test is the difference
between "we think nobody passes fast-math" and "nobody can".

## Holes this version ships with

Following the oxide precedent: ship holes, never a synthetic stub.

| Hole | Why | Would look like |
|---|---|---|
| **No graph capture** | `graph.rs` is cudarc-only; all 26 of its cfgs key on `feature = "cuda"` | HIP *does* have `hipGraph*`, so a later split into `graph_cuda.rs`/`graph_hip.rs` behind the same public type is real work, not a rewrite |
| **No allocation pool** | `AllocPool` is a `backend_cuda` type; every HIP buffer is a fresh `hipMalloc` | the oxide backend ships the same hole; `cuMemAlloc` measured 117 µs on an RTX 2060, so the motivation transfers |
| **No explicit `craton.gpu` API** | `GpuExecutor`/`GpuFuture`/`GpuArray`, `graphBeginCapture` stay CUDA-only | out of v1 scope, deliberately |
| **No GEMM built-in** | `vm/src/runtime/kernels/` loads a committed PTX blob | `OffloadCache::builtin_module` refuses; the natives fall back to the interpreter |
| **Linux only** | `cubecl-hip-sys` is Linux-only, and the Windows HIP SDK renamed its runtime DLL | [`hip-windows-runtime-dll-name.md`](../known-issues/gpu/hip-windows-runtime-dll-name.md) |
| **Partial lowering coverage** | the kernel IR is mid-migration; unstructured instructions are refused by name | `jit-hip/tests/golden/hip/manifest.txt` carries the number and names the next site worth converting |

That last row is the interesting one. `jit-hip` **refuses** any
instruction the front end has not expressed structurally yet, rather
than translating it on a guess — because a guessed translation does not
crash, does not trip the deopt flag and does not fall back to the CPU.
It returns a plausible wrong answer, which is this subsystem's
characteristic failure. The coverage number is in a file that has to be
re-recorded, so "the HIP backend works" cannot quietly come to mean "on
the four kernels somebody tried".

## Getting to hardware

`tools/gpu/amd-remote/` is the kit: one command copies the tree to a
Linux AMD box and runs six stages in ascending order of cost, each able
to fail only for reasons the ones before it have ruled out. Its README
explains the ordering and what to read when a stage fails.

`.github/workflows/gpu-selfhosted.yml` carries a commented `gpu-amd`
job so enrolling a runner is an edit of two lines.

**Which card.** An RDNA part is wavefront 32, which the host-shim
oracle already exercises on every PR. The uncovered case is wavefront
64 — CDNA and older GCN. If only one card can be had, that is the
argument for a 64-lane one.
