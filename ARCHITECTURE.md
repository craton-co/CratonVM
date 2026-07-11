# Architecture

This document describes the high-level architecture of CratonVM.
If you want to contribute, this is the place to start.

## Crate Layout

The workspace has 20 member crates (the `fuzz` harness is a separate,
standalone workspace, not a member):

```
cratonvm/
  reader/              cratonvm-reader              .class file parser
  types/               cratonvm-types               Shared types (Value, ClassId, ObjectRef)
  native-api/          cratonvm-native-api          NativeContext trait & FD table
  native-builtins/     cratonvm-native-builtins     java.lang.* native methods
  native-collections/  cratonvm-native-collections  java.util.* native methods
  native-io/           cratonvm-native-io           java.io/nio native methods
  native-awt/          cratonvm-native-awt          AWT/Swing/Java2D native peers
  jit-api/             cratonvm-jit-api             JIT compiler API types
  jit/                 cratonvm-jit                 x86-64 / AArch64 JIT compiler
  jit-cuda/            cratonvm-jit-cuda            Java bytecode -> PTX lowering for GPU offload
  cuda-bridge/         cuda-bridge                  Thin CUDA Driver API bridge for GPU offload
  craton-gpu/          craton-gpu                   Build-time-only: packages the @GpuKernel/@Parallel Java annotation sources into a jar for jit-cuda's build script; no runtime code
  classloading/        cratonvm-classloading        Class loading & bytecode verification
  gc/                  cratonvm-gc                  Garbage collectors (default generational semi-space; G1 region-based; experimental feature-gated zgc stub)
  jfr/                 cratonvm-jfr                 Java Flight Recorder
  vm/                  cratonvm-vm                  VM runtime engine
  vm-cli/              cratonvm-cli                 CLI entry point
  libcratonvm/         libcratonvm                  C-ABI shared library for embedding (cdylib/staticlib libjvm substitute)
  cratonvm-embed/      cratonvm-embed               Semver-stable Rust facade for embedding CratonVM
  difftest/            cratonvm-difftest            HotSpot differential-testing harness
```

The `fuzz/` directory is its own standalone workspace (`cratonvm-fuzz`, nightly-only libFuzzer harness) and is *not* a member of this workspace — its `#![no_main]` `fuzz_target!` expansion trips the production lints, so it builds separately via `cargo +nightly fuzz build`.

**Dependency flow:**
```
vm-cli -> vm -> {classloading, gc, jit, native-builtins, native-collections,
                 native-io, native-awt, jfr}
                 -> {reader, types, native-api, jit-api}
                 -> (gpu-offload feature only) {jit-cuda, cuda-bridge}
                     -> also forwards gc/gpu-offload, native-builtins/gpu-offload

libcratonvm  -> vm   (C-ABI / JNI Invocation API embedding shim)
cratonvm-embed -> vm (curated, semver-stable Rust embedding facade)
```

`craton-gpu` does not appear above: it is a *build-dependency* of
`jit-cuda` only (its `build.rs` compiles the GPU annotation sources and
exposes their jar path via Cargo's `links` metadata), never a runtime
dependency of anything.

## reader — Class File Parser

Parses `.class` files per the JVM specification (JVMS Ch. 4).

```
reader/src/
  class_reader.rs       Entry point: bytes -> ClassFile
  buffer.rs             Binary cursor (big-endian reads)
  constant_pool.rs      20 CP entry types
  instruction.rs        200+ bytecode opcodes
  attribute.rs          30+ attribute types
  stack_map.rs          StackMapTable verification frames
  field_type.rs         Field descriptor parsing (e.g. "Ljava/lang/String;")
  method_descriptor.rs  Method descriptor parsing (e.g. "(II)V")
  class_access_flags.rs Bitflag types for access modifiers
  class_file_version.rs Major/minor version constants (Java 1.1-25)
```

The reader is a pure parser with no VM dependencies. It can be used
independently to inspect `.class` files.

## vm — Virtual Machine

The VM is the core of the project (~323,000+ LoC across 20 workspace member crates, plus the separate `fuzz` harness workspace). It contains six
major subsystems (several now extracted into their own crates):

### Runtime (`vm/src/runtime/`)

The bytecode execution engine.

- **`interpreter.rs`** — Main dispatch loop. 140+ fast-path opcodes via
  a match on the bytecode. Each opcode reads operands, manipulates the
  operand stack and local variables, and advances the program counter.
- **`frame.rs`** — Stack frame: local variables + operand stack, stored
  in SoA (Structure-of-Arrays) layout for cache efficiency.
- **`call_stack.rs`** — Per-thread call stack of frames.
- **`value_stack.rs`** — Typed operand stack (SoA encoded).
- **`exceptions.rs`** — Java exception creation and throw handling.
- **`invokedynamic.rs`** — Lambda/method-ref bootstrap via LambdaMetafactory.

### Class Loading (`classloading/` crate)

Implements JVMS Ch. 5: loading, linking, and initialization. Extracted into the
`cratonvm-classloading` crate.

- **`class_manager.rs`** — Central class cache and loading coordinator.
- **`loaders.rs`** — Bootstrap, extension, and application class loaders.
- **`class_path.rs`** — Classpath scanning (directories + JARs via `zip` crate).
- **`class.rs`** — Runtime class representation with metadata and hierarchy.
- **`verifier.rs`** — Structural verification (JVMS 4.8-4.9).
- **`bytecode_verifier.rs`** — Type-checking verification (JVMS 4.10).
- **`vtype.rs`** — Verification type lattice.
- **`resolution.rs`** — Symbolic reference resolution with inline caching.
- **`access_control.rs`** — Access checking (JVMS 5.4.4).

### Memory (`gc/` crate)

Garbage collectors, extracted into the `cratonvm-gc` crate. The default is the generational semi-space collector (Cheney moving young gen + non-moving sweep). A region-based G1 collector is also present and opt-in selectable via `-XX:+UseG1GC` (experimental; Generational remains the default safety net during G1 maturation — see `docs/feature-designs/concurrent-gc-maturation.md`). ZGC is experimental and feature-gated (`--features zgc`, off by default): a metadata-only simulation plus a real STW mark-sweep heap (`ZgcRealHeap`) that is built but not yet wired into the backend dispatch (`GcBackend`), so neither is a selectable production collector.

- **`heap.rs`** — Object/array layout and allocation (semi-space).
- **`gen_heap.rs`** — Generational heap: young gen (copying) + old gen.
- **`gc.rs`** — Cheney copying collector algorithm.
- **`collector.rs`** — GC coordination and stop-the-world orchestration.
- **`card_table.rs`** — Card marking for cross-generational references.
- **`arena.rs`** — Bump-pointer memory arena.
- **`roots.rs`** — Root scanning and pointer remapping.
- **`old_gen.rs`** — Old generation management.

**Object layout:**
```
[ObjectHeader (32 bytes)] [field0 (16 bytes)] [field1 (16 bytes)] ...
```

Arrays use compact element sizes (1/2/4/8 bytes per element depending on type).

### JIT Compiler (`jit/` crate)

Custom x86-64 / AArch64 JIT compiler (~7,200 LoC). Extracted into the `cratonvm-jit`
crate, with shared API types in `cratonvm-jit-api`.

- **`lib.rs`** — JIT infrastructure: compiled code cache, OSR entry points.
- **`x64.rs`** — x86-64 machine code emitter with 26 optimization rounds.
- **`aarch64.rs`** — AArch64 machine code backend (partial coverage).
- **`ir.rs`, `ir_optimize.rs`, `ir_schedule.rs`, `ir_lower.rs`** — optional
  sea-of-nodes IR pipeline (build, optimize, schedule, lower to x64).

**Compilation pipeline:** the JIT has two paths. The default is a
single-pass emitter that lowers bytecode directly to x86-64 native code
(`x64::compile`). Methods that pass `ir_compatible()` instead go through
an optional sea-of-nodes IR pipeline
(`bytecode -> IrBuilder -> Graph -> optimize -> schedule -> lower -> x64`,
in `ir.rs`, `ir_optimize.rs`, `ir_schedule.rs`, `ir_lower.rs`), which
decouples optimization from instruction selection; others fall back to the
direct single-pass path.

Key optimizations: register allocation for locals, magic division,
LICM, bounds check elimination, AVX2 SIMD, on-stack replacement (OSR).

Methods are compiled after 100 invocations (configurable). Two calling
conventions: **pure** methods (direct call) and **context** methods
(receive `SharedVm` pointer as hidden first argument).

### Native Methods (`native-builtins/`, `native-collections/`, `native-io/` crates)

3,100+ synthetic implementations of Java standard library methods, split across
domain-specific crates. The `NativeContext` trait lives in `native-api/`.

- **`native-builtins/`** — java.lang.* native methods.
- **`native-collections/`** — java.util.* native methods.
- **`native-io/`** — java.io/nio native methods.

Instead of loading `rt.jar`, CratonVM provides native Rust implementations
of JDK classes. The `NativeContext` trait (in `native-api/`) provides a
VM-agnostic interface for native methods to access the heap, class manager,
and thread state.

### Threading (`vm/src/threading/`)

- **`jvm_thread.rs`** — Per-thread state (call stack, printed output).
- **`thread_registry.rs`** — Global thread tracking.
- **`monitor.rs`** — Object monitors (synchronized/wait/notify).
- **`gc_barrier.rs`** — Stop-the-world safepoint coordination.
- **`virtual_scheduler.rs`** — Virtual thread scheduler (Java 21+).

## GPU Offload (opt-in, `--features gpu-offload`)

Entirely feature-gated: a default build links none of this and the
interpreter's hot path carries zero extra branches. Gating features:
`gpu`/`gpu-driver` on `cratonvm-cli`, `gpu-offload` on `cratonvm-vm` (which
forwards to `cratonvm-gc` and `cratonvm-native-builtins`). See
[BUILD_GUIDE.md](BUILD_GUIDE.md#building-with-gpu-offload) for the build
levels and [docs/gpu/README.md](docs/gpu/README.md) for the full reference.

**Crates.** `jit-cuda` lowers Java bytecode to PTX: `analyzer.rs` decides
whether a static method is GPU-eligible (primitives only, no allocation,
calls, fields, or reference arrays), `lowering.rs` / `loop_recog.rs` /
`emit.rs` turn an eligible method's counted loop into PTX text. `cuda-bridge`
is a thin CUDA Driver API wrapper with a no-driver `backend_stub.rs`
default and a real `backend_cuda.rs` (behind `cuda-bridge/cuda`) built on
`cudarc`.

**Pipeline.** The interpreter's `execute_invokestatic` hook
(`vm/src/runtime/interpreter.rs`) calls `runtime::offload::try_dispatch`
(`vm/src/runtime/offload.rs`), which asks the per-VM `OffloadCache` to
analyze-and-lower a callee once and cache the resulting `CompiledKernel`
(a loaded PTX module) by `(ClassId, method_index)`. On a cache hit,
`dispatch_method_from_native` / `dispatch_async` marshal the Java
primitive-array arguments — via `vm/src/runtime/gpu_marshal.rs`'s
`host_view_<T>`/`write_back_<T>` packed-copy path, or a zero-copy DMA
straight against the JVM heap arena once an array's element type is
proven — then hand them to `cuda-bridge`'s `DeviceContext`. The context
runs the upload, launch, and download on three CUDA streams (`copy_h2d`,
`compute`, `copy_d2h`) ordered by CUDA events rather than a blocking sync
per stage. Kernels signal failure (e.g. an out-of-bounds index) by writing
a device `failure_flag` word instead of throwing; `finalize_submission`
checks it once the event chain completes and deopts to the CPU
interpreter — leaving no partial GPU state in the heap — on a failure,
or copies results back and resumes the Java frame on success.
`vm/src/runtime/gpu_residency.rs` separately tracks longer-lived
`GpuArray<T>` host/device residency for the explicit async API, independent
of this transparent per-call path.

**GC coordination.** While kernel arguments are in flight, the calling
thread holds a `SafepointToken` from `Heap::enter_gpu_critical()` and pins
the argument `ObjectRef`s via `Heap::pin_ref` (both in `gc/src/heap.rs`).
The collector checks the resulting `gpu_critical_count` and yield-spins
rather than moving or reclaiming while any token is alive; the root walker
visits pinned refs so a kernel never reads through a stale or relocated
pointer.

## vm-cli — Command-Line Interface

Thin wrapper (~200 LoC) using `clap` for argument parsing. Constructs a
`Vm`, calls `main(String[])`, and handles exit codes.

## Key Design Decisions

1. **No JDK dependency.** All standard library classes are implemented as
   native methods in Rust. This means no `JAVA_HOME`, no `rt.jar`, and
   no dependency on any JDK installation at runtime.

2. **Two-layer exception model.** `MethodCallFailed::ExceptionThrown` wraps
   Java-catchable exceptions; `MethodCallFailed::InternalError` wraps VM
   bugs. The interpreter catches the former at catch/finally blocks.

3. **SoA value layout.** Frames and operand stacks store tags and payloads
   in separate arrays for better cache utilization during GC scanning.

4. **Direct bytecode-to-x86-64, with an optional IR.** The JIT's default
   path compiles bytecode directly to machine code in a single pass, which
   keeps that path simple at the cost of limiting cross-instruction
   optimizations. Methods that qualify (`ir_compatible()`) are instead
   routed through an optional sea-of-nodes IR that enables broader
   optimization before instruction selection.

5. **FNV-1a native dispatch.** Native methods are looked up by hashing
   `"class_name.method_name:descriptor"`. O(1) dispatch with collision
   detection at registration time.

## Data Flow

```
.class bytes
    |
    v
  reader::read_class()     parse into ClassFile
    |
    v
  ClassManager::load_class()  link, verify, prepare
    |
    v
  interpreter::execute()   run bytecode
    |   ^
    |   | (hot method threshold)
    v   |
  jit::compile()           emit x86-64, cache
    |
    v
  native call              compiled code calls back into VM
```

## Testing Strategy

- **Unit tests** live in `#[cfg(test)]` modules alongside production code.
- **Integration tests** in `vm/tests/` run compiled `.class` files through
  the full VM pipeline.
- **Java test classes** in `test_classes/` and `vm/tests/resources/` are
  compiled by `build.rs` if `javac` is available.
- **CI** (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo build`,
  `cargo clippy`, and `cargo test` across the workspace on Linux and Windows.
