# GPU-on-JVM: CratonVM vs. the field

This document positions CratonVM's GPU-offload feature next to the four
most-cited Java-on-GPU solutions in the open ecosystem (as of May 2026):

1. **TornadoVM** — the most mature OSS heterogeneous-programming framework
   for the JVM.
2. **Project Babylon (HAT)** — OpenJDK incubator project building "code
   reflection" primitives, with the Heterogeneous Accelerator Toolkit
   (HAT) as the first GPU consumer.
3. **Aparapi** — long-standing OpenCL-backed framework, originally from
   AMD, now community-maintained under Syncleus.
4. **IBM SDK for Java / Semeru** — IBM's Java distribution that ships a
   narrow `com.ibm.gpu` API for offloading specific operations (notably
   `Arrays.sort`) to NVIDIA GPUs via CUDA.

Each row in the table below is one of the five solutions. Columns
follow the 15 comparison dimensions the brief asks for. Where public
sources are inconsistent or silent, the cell says **"unclear from
public docs"** rather than guessing. Performance numbers cite the
canonical kernel they were measured on. CratonVM's row carries real RTX 2060 measurements (warm, full
H2D + kernel + D2H per call, checksum-verified against HotSpot):
**~210× over HotSpot C2 and ~3× over TornadoVM PTX** on the
data-dependent integer-division kernel, **178–472× over CratonVM's
own CPU JIT** on the 96-MAD kernel. See `bench-gpu/results/` and the
repo README for the full tables; the old "5.18× with JIT disabled"
figure below is superseded.

---

## The comparison table

| Dimension | **CratonVM** | **TornadoVM** | **Project Babylon / HAT** | **Aparapi** | **IBM SDK Java / Semeru** |
|---|---|---|---|---|---|
| **1. OSS license / governance** | Apache 2.0, single-vendor (craton-co) — see `LICENSE` at the repo root. | Dual-licensed: Apache 2.0 for API / annotations / examples, **GPL v2 for runtime + drivers** ([github.com/beehive-lab/TornadoVM](https://github.com/beehive-lab/TornadoVM)). Originally from the University of Manchester Beehive Lab. | **GPL-2.0 with Classpath Exception** (OpenJDK fork) ([github.com/openjdk/babylon](https://github.com/openjdk/babylon)). Governed by OpenJDK. | **Apache 2.0**, community-maintained under Syncleus ([github.com/Syncleus/aparapi](https://github.com/Syncleus/aparapi)). | Mixed: Semeru Runtime is open (GPL-2.0 + CE / EPL via OpenJ9), but the historical `com.ibm.gpu` GPU-offload code shipped in the **proprietary IBM SDK for Java 8** ([ibm.com com.ibm.gpu docs](https://www.ibm.com/docs/en/sdk-java-technology/8?topic=only-comibmgpu-application-programming-interface-linux-windows)). Whether it carried forward into modern Semeru is unclear from public docs. |
| **2. Underlying JVM** | **Replaces the JVM entirely.** The interpreter, JIT, GC, and native bridge are all written in Rust (~323k LoC). No HotSpot, no GraalVM, no OpenJDK; ships its own synthetic JDK class implementations. | **Plugin / extension** on top of OpenJDK + GraalVM (requires GraalVM-based JDK 21 or JDK 25 per [TornadoVM 4.0.1 release notes](https://github.com/beehive-lab/TornadoVM/releases)). Uses Graal as its compiler backbone. | **OpenJDK fork** — an incubator branch of OpenJDK itself. The intent is for code reflection to eventually merge into mainline OpenJDK as a series of JEPs ([openjdk.org/projects/babylon](https://openjdk.org/projects/babylon/)). | **Library** that runs on any OpenJDK / OracleJDK / Semeru. Uses JNI to call into OpenCL. | **Specific JDK distribution.** Historically the IBM SDK for Java 8; today IBM Semeru Runtime (based on Eclipse OpenJ9 + OpenJDK class libraries). |
| **3. GPU backend(s)** | **CUDA only.** PTX text emitted from the analyzer; loaded via `cudarc 0.13.9` Rust bindings; CUDA Toolkit 12.6 pinned. NVIDIA only. (See [`docs/gpu/README.md`](./README.md).) | **OpenCL, NVIDIA PTX, SPIR-V, and Apple Metal** (per release notes & [README.md](https://github.com/beehive-lab/TornadoVM/blob/master/README.md)). Targets Intel/NVIDIA/AMD/Apple Silicon GPUs and Intel/Xilinx FPGAs. | **OpenCL C, CUDA PTX, SPIR-V** (SPIR-V in progress) — HAT lowers from Babylon's code model to all three. | **OpenCL** only (1.2/2.0/2.1). | **CUDA** only (NVIDIA on POWER8 / select Linux + Windows). |
| **4. Source language for kernels** | **Plain Java** static methods (or non-static methods using only `this.field` array refs, as of Phase 9 #2). User may annotate with `@GpuKernel`, `@GpuExclude`, `@EnableGpuAsync` to steer the analyzer; annotations are advisory, not mandatory. | **Plain Java with `@Parallel` / `@Reduce` annotations** (loop-parallel API), **or** an explicit `KernelContext`-based kernel API. Off-heap typed arrays (`IntArray`, `FloatArray`, …) replace `int[]` / `float[]`. | **Plain Java methods annotated with `@CodeReflection`** (formerly `@Reflect`). HAT layers an explicit `NDRange` / `KernelContext` SIMT API on top — closer to CUDA C than to `parallelStream`. | **Subclass `com.amd.aparapi.Kernel` and override `run()`**; or use the lambda-style `Range.create(n).execute(i -> …)` introduced in 2.x. | **No kernel surface.** User calls `com.ibm.gpu.Maths.sortArray(int[])` (and friends). The user does **not** author a kernel — IBM ships pre-canned CUDA kernels for a fixed set of operations. |
| **5. How the kernel becomes GPU code** | **Runtime bytecode → PTX text → cuModuleLoad.** `jit-cuda/` analyzes the bytecode, recognises the counted-loop pattern, walks opcodes simulating the operand stack with PTX virtual registers, emits PTX text, and `DeviceModule::from_ptx` loads it. (See [`docs/gpu/README.md`](./README.md) §2.) | **Runtime JIT compilation** via Graal: Java bytecode → Graal IR → OpenCL C / PTX assembly / SPIR-V binary, then loaded onto the device. | **Code Reflection** generates a structured code model (close to an AST + types + CFG) at `javac` time and stores it in the class file. HAT consumes that model at runtime and lowers it to OpenCL C / CUDA PTX / SPIR-V. | **Runtime bytecode → OpenCL C source string → `clBuildProgram`.** Aparapi inspects the `Kernel.run()` bytecode at first invocation and translates it to OpenCL C text. | **AOT-shipped CUDA binaries.** The user-visible API calls pre-built CUDA code inside the JDK; no user-Java is converted to GPU code. |
| **6. API style** | **Two coexisting paths.** *Transparent:* `--gpu` flag intercepts eligible static `invokestatic` calls at the interpreter hook. *Explicit:* `craton.gpu.GpuExecutor.open()` + `submit("ClassName", "methodName", "(descriptor)", args…)` returning `GpuFuture<T>`. Also `submit(Supplier)` for static method-reference lambdas. (See [`docs/gpu/async-api.md`](./async-api.md).) | **`TaskGraph` builder** + `TornadoExecutionPlan` driver. User constructs a graph of tasks (`task("name", Class::method, args)`), transfers data, executes. | Explicit kernels with `NDRange` + `KernelContext` (thread IDs, group-local memory). User must understand SIMT. | `Kernel.execute(Range)` or `Range.create(n).execute(i -> …)`. Kernel state is held in fields of the `Kernel` subclass; arrays passed via field. | `Arrays.sort` / `com.ibm.gpu.Maths.sortArray(...)` — fixed function calls. **No DSL.** |
| **7. Sync model** | `GpuFuture<T>` (`get()`, `getNow()`, `isDone()`, `thenApplyGpu()`); transparent path is synchronous. Each `GpuExecutor` owns one default `GpuStream`; users can create additional streams via `newStream()`. CUDA events back the futures internally. A timed `get(timeout, unit)` landed in 0.4.0, backed by the bridge's blocking wait where one exists; `cancel()` reaches the device but is always refused, because there is no device-side cancellation primitive yet (per the [gpu4j README](https://github.com/craton-co/gpu4j)). | Synchronous `execute()` on the `TornadoExecutionPlan`; asynchronous launch via separate API. CUDA events used internally between tasks within a graph. | Per-launch synchronous in current samples; Babylon itself is a code-model project, not an async-runtime project. Async layering is the consumer library's job (HAT, in this case). | `Kernel.execute(...)` is synchronous; an async variant exists but is rarely used. | Synchronous function call. |
| **8. Memory model** | **Explicit `GpuArray<T>`** for device-resident handles + an automatic-marshal path for the transparent flow. Phase 9 #1 added **deferred D→H writeback**: arrays stay dirty on device until `arrayToHost` (or `GpuArray.toHost()`) demands them. GC is paused via `SafepointToken` + `Heap::pin_ref` while a kernel runs. No unified memory. | **Off-heap typed segments** (`IntArray`, `FloatArray`, ...) built on the Foreign Memory API; user explicitly declares `DataTransferMode.FIRST_EXECUTION` etc. Auto-copy by default; user can opt into device-resident chaining. | Explicit. HAT defines its own buffer types (`F32Array` etc.) and the user marshals via `iFaceMapper` ([jfumero.dev Babylon vs Tornado](http://jfumero.dev/posts/2025/02/07/babylon-and-tornadovm)). | Auto-copy of `Kernel` instance fields on each `execute()`; explicit `put()`/`get()` for advanced cases. Easy but expensive on chained kernels. | Hidden inside the IBM-shipped function. User passes a plain `int[]`; IBM does the H→D copy, runs the canned kernel, copies back. |
| **9. Eligibility / static analysis** | **Static analyzer in `jit-cuda/src/analyzer.rs`** rejects: non-static methods (unless Phase 9 #2 `this.field`-only opt-in), synchronized, native/abstract, non-primitive params, allocation, calls, fields, type checks, monitors, switches, throws, jsr/ret, reference arrays. Estimates work via backward-branch count and skips offload below `--gpu-min-work` (default 4096). **Also accepted:** `ldc`/`ldc_w`/`ldc2_w` constant-pool loads (int/float/long/double literals), `frem`/`drem` (opt-in, `ALLOW_DIV_BY_ZERO`), `lcmp`/`fcmp*`/`dcmp*` value-form comparisons (a compare feeding a branch still rejects at the branch — no new false eligibility), non-zero-`ldc`-sourced-start counted loops, and a curated `Math`/`StrictMath` intrinsics table (`sqrt`(double)/`abs`/`min`/`max`(int/long/float/double)/`fma`(float/double), opt-in `ALLOW_INTRINSIC_CALLS`) are now admitted *and* lowered — see `docs/gpu/annotations.md`. Still rejected: 2-D/nested loops, general (non-loop-guard) branches, `sin`/`cos`/`exp`/`log`/`pow`. | Graal-based analysis. Eligibility much broader than CratonVM — method calls, more control-flow shapes, FP intrinsics are admitted. Off-heap segment types replace plain Java arrays. | Code Reflection captures the full AST at compile time, so HAT does whole-method analysis. Practical eligibility is whatever HAT's lowering passes can handle today (matmul, vector add, etc.); under active development. | Bytecode walker. Failures are diagnostic strings at first `execute()` — no static gate before submission. Real-world support is narrower than the docs suggest (lots of Java features generate OpenCL that won't compile). | N/A — only canned operations. |
| **10. Performance (canonical kernel)** | **5.18× best, 6× mean over CPU on vector-add n = 2²⁰** (user-reported, JIT disabled to dodge a known int[]-loop JIT regression — see Caveat below). No matmul / saxpy numbers landed yet; [`docs/gpu/first-results.md`](./first-results.md) lists the acceptance scaffolding. | Published claims include **up to 270× on NVIDIA 1050 vs sequential Java**, **6.61× on KernelContext matmul 2048×8192**, and a **778× GPU-vs-CPU matmul** ([TornadoVM docs / InfoQ](https://www.infoq.com/articles/java-performance-tornadovm/)). Numbers depend heavily on workload and hardware. | Few published cross-platform numbers; HAT is pre-production. Babylon-vs-TornadoVM matmul comparisons cited TornadoVM as **2.3–9.3× faster than Babylon** ([jfumero.dev](http://jfumero.dev/posts/2025/02/07/babylon-and-tornadovm)). | Historic AMD-era numbers in the 10–50× range for embarrassingly parallel kernels; not measured against modern CPUs / JITs recently. | IBM documents **multi-× speedups** for `Arrays.sort` on large arrays on POWER8 + NVIDIA — no canonical CPU-vs-GPU number tabulated in current docs. |
| **11. Maturity / last release** | **Pre-1.0** (`gpu4j-core` Java artefact 0.4.0; the repository and artefacts were renamed from `craton-gpu-java`/`craton-gpu` on 2026-09-06). Active development; recent commits in May 2026 include Phase 5–9 GPU work (`b6c9a73`, `1d8348d`, `5a47853`, `7c569ec`). Single contributor org. Real-GPU validation in progress; first-results table not yet populated for n = 2²⁴. | **Mature, production-OSS.** TornadoVM 4.0.1-jdk25 released 2026-04-29, 4.0.1-jdk21 on 2026-04-29, 2.0 in 2024 ([Phoronix](https://www.phoronix.com/news/TornadoVM-2.0-Released)). Used in research and some industry pilots. Active commit cadence. | **Incubating in OpenJDK.** Not shipping in JDK 26 or 27 — will land as a series of JEPs across future feature releases. Talks at JVMLS 2025, Devoxx 2025, JavaOne 2025 indicate active work. ([openjdk.org Babylon HAT article](https://openjdk.org/projects/babylon/articles/hat-matmul/hat-matmul)). | **Largely dormant.** Last release v3.0.0 on **2016-07-12**; v2.0.0 in 2020 ([github.com/Syncleus/aparapi/releases](https://github.com/Syncleus/aparapi/releases)). Some community activity but no recent releases in 2024–2026. | The `com.ibm.gpu` API was documented for **IBM SDK Java 7R1 (2014) and Java 8**. Current status in modern Semeru releases unclear from public docs; appears to have been a POWER8-era feature that did not migrate prominently to Semeru. |
| **12. Platforms** | **Linux + Windows x86_64** for CratonVM; CUDA bridge has been built/tested with `cudarc 0.13.9` against CUDA Toolkit 12.6 on Windows 11. No macOS GPU path (CUDA is NVIDIA-only). aarch64 host: builds, but no CUDA driver on ARM Linux desktops. | **Linux x86_64, Windows x86_64, macOS aarch64** (per [TornadoVM 4.0.1 release notes](https://github.com/beehive-lab/TornadoVM/releases)). Backend per platform: OpenCL/PTX/SPIR-V on Linux+Win, Metal on macOS Apple Silicon. | Wherever OpenJDK builds — runtime support follows HAT backends (OpenCL/CUDA on Linux+Win; SPIR-V in progress). | Linux 32/64-bit, Windows 32/64-bit, macOS 64-bit (older — predates Apple Silicon). | Historically Linux on POWER8 + select Linux/Windows x86 with NVIDIA. Not macOS / not Apple Silicon. |
| **13. Strengths** | Whole stack is one cohesive Rust codebase — no HotSpot dependency, no GraalVM dependency, no JNI hops to OpenCL; `--features gpu-driver` either compiles in or compiles out cleanly with zero hot-path branches in the CPU build. | The most mature, most-targeted (CPU/GPU/FPGA/multi-vendor), most-tested OSS solution; broad API; production users. | Backed by the OpenJDK process; long-term direction-of-travel for Java GPU support; AST-level code models open the door to many lowering targets (SQL, Triton, ONNX, …). | Simple mental model; runs on any JVM; OpenCL covers AMD/Intel/NVIDIA. | Zero-effort offload for the user — `Arrays.sort` "just gets faster" on a supported box. |
| **14. Weaknesses / known limitations** | Narrow analyzer (static methods, primitive arrays, counted loops only); NVIDIA/CUDA-only; pre-1.0 with no real-GPU benchmark suite landed; transparent path conflicts with one JIT bug on int[] iteration (workaround: `--nojit`); the whole JVM is not yet a HotSpot drop-in either, which is a separate maturity story. | Heavy stack (Graal + LLVM-style passes); GPL on the runtime; requires GraalVM-based JDK; off-heap typed arrays require user code changes. | Pre-incubator-graduation — not shipping in any released JDK; HAT API is explicit SIMT (NDRange/KernelContext), so requires CUDA-style mental model from the user. | Effectively unmaintained; OpenCL-only at a time when the ecosystem (especially Apple, NVIDIA HPC, Windows) has moved toward Metal/CUDA/SYCL. | Closed surface: only what IBM shipped is offloaded. No user-authored kernels. POWER8-centric. |
| **15. When you'd pick this** | You're already running on CratonVM (or you want a Rust-native JVM), and your hot path is a tight elementwise loop over primitive arrays you can express as static methods. The transparent `--gpu` path is the lowest-effort GPU on-ramp for that exact shape. | You want the most-mature OSS heterogeneous-Java solution today, you can ship a GraalVM-based runtime, and you need OpenCL/CUDA/SPIR-V/Metal coverage. | You want to invest in the long-term mainline Java direction and you're comfortable with an explicit SIMT kernel API. Code Reflection itself is independently useful (SQL DSLs, autodiff, ML lowering). | You have a legacy Aparapi codebase and don't want to migrate. Otherwise hard to justify in 2026. | You're already on the IBM stack and your bottleneck is `Arrays.sort` on huge primitive arrays. |

---

## Honest positioning of CratonVM

**What CratonVM does that the others don't.** CratonVM is the only entry
in this table where the entire JVM — interpreter, x86-64 JIT, generational
GC, native bridge, *and* the GPU offload path — is one cohesive Rust
codebase. Every other solution is either a plug-in/extension on top of
OpenJDK (TornadoVM, Aparapi), a fork of OpenJDK (Babylon), or a
distribution-specific feature buried in a particular JDK build (IBM SDK).
The practical consequence is that CratonVM's CPU-only build is
*byte-identical* to its pre-GPU state — `--features gpu-driver` is a
single Cargo flag with zero hot-path branches in the interpreter when off.
That kind of cfg-discipline is hard to retrofit onto HotSpot. The
automatic `--gpu` path also distinguishes CratonVM: within the analyzer's
supported shape the user writes ordinary `int[]` static methods with no
annotations, and the interpreter hook decides per-invocation. TornadoVM
requires a `TaskGraph`; Babylon/HAT requires `@CodeReflection` + `NDRange`;
Aparapi requires subclassing `Kernel`; IBM requires `Arrays.sort`. CratonVM
requires a `gpu-driver` build and the `--gpu` flag, but nothing at the call
site — and it pays for that with a far narrower eligible set than any of
them (row 9), with everything else falling back to the CPU.

**What others do that CratonVM doesn't.** Almost everything else, frankly.
TornadoVM ships four GPU backends (OpenCL, PTX, SPIR-V, Metal) plus FPGA
support; CratonVM ships one (CUDA PTX) and only on NVIDIA. TornadoVM has
been in production since 2020, has published numbers across LLM inference
and matmul on Apple Silicon, and has Graal-grade IR-level optimisation;
CratonVM's PTX emitter is a stack-simulator that handles counted loops
and rejects most other shapes. Babylon comes with the gravity of OpenJDK
governance and a long-horizon AST-based code model that opens the door
to non-GPU targets (SQL, autodiff). IBM's `com.ibm.gpu.Maths.sortArray`
is one method call away — no analyzer, no annotations, no kernel author.
CratonVM's published research footprint is one repo;
TornadoVM has a decade of published research.

**On CratonVM's numbers.** The old "5.18× with the JIT
disabled" caveat is obsolete: transparent `--gpu` offload was measured on an
RTX 2060 with the JIT on, warm, full per-call H2D + kernel + D2H, checksums
matching HotSpot bit-for-bit at every size. Headlines: div-chain kernel
(48 data-dependent integer divisions/element, unvectorizable on x86) —
CratonVM-GPU 9 ms vs HotSpot C2 1,910 ms vs TornadoVM 28 ms at n = 2²⁴;
96-MAD kernel — CratonVM-GPU 27 ms vs TornadoVM 51 ms at n = 2²⁶. Full
tables in `bench-gpu/results/` and the repo README.

**Reduction support and eligibility expansion.**
CratonVM's transparent `--gpu` path now offloads integer/long reduction
kernels (`sum += a[i] * b[i]`-shaped methods returning `)I`/`)J`), which is
directly comparable to TornadoVM's `@Reduce` annotation — and, on this box,
CratonVM handles a reduction shape TornadoVM's own PTX backend does not: the
dot-product-over-`int[]` `@Reduce`-into-`LongArray` shape
(`bench-tornado/TornadoDotBench.java`, built by mirroring TornadoVM's shipped
`ReductionAddFloats` example) throws `TornadoInternalError: unimplemented`
against TornadoVM 4.0.1's PTX backend on this hardware. CratonVM's own numbers
on the same kernel are honest rather than flattering: GpuDotBench at N = 2²⁴
measured CratonVM-GPU 18 ms vs CratonVM-CPU 76 ms (4.2× — the GPU-vs-own-CPU
win the feature is actually about) vs HotSpot C2 7 ms (the GPU does **not**
beat vectorized HotSpot here; the kernel is PCIe-bound plus single-cell atomic
contention at this size). `)F`/`)D` reductions remain CPU-only by design — GPU
float atomic-add is not bit-identical to Java's sequential fp accumulation.
Separately, the analyzer/lowering gap that made `ALLOW_INTRINSIC_CALLS`
non-functional is closed: a curated `Math`/`StrictMath` table (`sqrt` on
`double`, `abs`/`min`/`max` on `int`/`long`/`float`/`double`, `fma` on
`float`/`double`) now resolves and lowers for real, narrower than TornadoVM's
broader FP-intrinsic support (row 9) but no longer vaporware. See
`docs/gpu/annotations.md` for the full detail.

---

## Sources

- TornadoVM: [github.com/beehive-lab/TornadoVM](https://github.com/beehive-lab/TornadoVM),
  [releases](https://github.com/beehive-lab/TornadoVM/releases),
  [readthedocs](https://tornadovm.readthedocs.io/en/latest/programming.html),
  [Phoronix 2.0 article](https://www.phoronix.com/news/TornadoVM-2.0-Released),
  [InfoQ perf article](https://www.infoq.com/articles/java-performance-tornadovm/).
- Project Babylon / HAT: [openjdk.org/projects/babylon](https://openjdk.org/projects/babylon/),
  [github.com/openjdk/babylon (code-reflection branch)](https://github.com/openjdk/babylon/tree/code-reflection),
  [hat-matmul article](https://openjdk.org/projects/babylon/articles/hat-matmul/hat-matmul),
  [JVMLS 2024 HAT slides (PDF)](https://cr.openjdk.org/~psandoz/conferences/2024-JVMLS/JAVA_BABYLON_HAT-JVMLS-24-08-05.pdf),
  [jfumero.dev Babylon vs TornadoVM](http://jfumero.dev/posts/2025/02/07/babylon-and-tornadovm),
  [inside.java JavaOne 2026](https://inside.java/2026/04/26/javaone-hat-java-gpu/).
- Aparapi: [github.com/Syncleus/aparapi](https://github.com/Syncleus/aparapi),
  [releases page](https://github.com/Syncleus/aparapi/releases),
  [aparapi.github.io](https://aparapi.github.io/).
- IBM SDK / Semeru: [com.ibm.gpu API docs](https://www.ibm.com/docs/en/sdk-java-technology/8?topic=only-comibmgpu-application-programming-interface-linux-windows),
  [GPU sort developer guide](https://www.ibm.com/support/knowledgecenter/en/SSYKE2_8.0.0/com.ibm.java.80.doc/docs/gpu_developing_sort.html),
  [CUDA-on-POWER8 overview](https://www.ibm.com/support/pages/introduction-nvidia-cuda-ibm-power8),
  [Semeru downloads](https://developer.ibm.com/semeru-runtime-downloads/).
- CratonVM (this repo): [`docs/gpu/README.md`](./README.md),
  [`docs/gpu/annotations.md`](./annotations.md),
  [`docs/gpu/async-api.md`](./async-api.md),
  [`docs/gpu/first-results.md`](./first-results.md),
  [gpu4j README](https://github.com/craton-co/gpu4j).
