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

The VM is the core of the project (~1,290,000 LoC across the 20 workspace
member crates as of 2026-07-25, plus the separate `fuzz` harness workspace).
It contains six major subsystems (several now extracted into their own
crates).

Rough size distribution, largest first, so newcomers know where the mass
actually is:

| Crate | LoC | Crate | LoC |
|-------|----:|-------|----:|
| `native-builtins` | 565,000 | `native-awt` | 17,000 |
| `vm` | 319,000 | `native-api` | 15,000 |
| `jit` | 97,000 | `jfr` | 14,000 |
| `gc` | 58,000 | `reader` | 13,000 |
| `native-collections` | 53,000 | `jit-cuda` | 10,000 |
| `native-io` | 50,000 | `types` | 9,500 |
| `classloading` | 50,000 | remaining 6 | < 6,000 each |

Several individual files are far larger than is comfortable. The two largest
*production* files are `vm/src/runtime/interpreter.rs` (~46,800 lines) and
`jit/src/x64.rs` (~41,000).

`vm/src/vm.rs` appears to dwarf both at 74,043 lines, but that number is
misleading and this is **not** the file to start reading. Everything from
line 62 onward is a single `#[cfg(all(test, feature = "synthetic-jdk"))]
mod tests` — a test module the default build does not even compile, because
`synthetic-jdk` is deliberately not a default Cargo feature (`vm/Cargo.toml`).
The orchestrator itself is the ~61-line module header above it, which declares
and re-exports `vm/src/vm/`: `vm_exec.rs` (~21,200 lines), `vm_init.rs`
(~11,900), `vm_util.rs` (~4,300), `vm_object.rs` (~2,100), and `realms/`.
Those are the files to open.

Two `native-builtins` files were worse and were split on 2026-07-25:
`lib.rs` went ~90,000 → ~39,000 across 13 per-domain
modules (`util_concurrent_ext`, `antlr_intrinsics`, `regex_matcher`,
`math_bignum`, …), and `phases_late.rs` went ~77,000 → ~8,000 across 18
modules under `native-builtins/src/phases_late/` (`nio_file`, `bouncycastle`,
`concurrent`, `ssl_security`, `net_channels`, `streams`, `jdbc`, …).
`phases_late.rs` itself now holds only the shared preamble, the per-phase
dispatchers, and cross-domain leftovers.

**Splitting a file does not speed up incremental builds, and it cannot.** Rust's
compilation unit is the *crate*, not the file: moving code into modules of the
same crate leaves that crate — and everything downstream of it — rebuilding in
full. Measured over the 90k→39k split above, `touch lib.rs && cargo build
--release -p cratonvm-cli` went 1m58s/2m00s before to 2m03s/2m03s after, i.e.
noise. The payoff of splitting is **merge-conflict surface and reviewability**
(the two worst files are 57% and 89% smaller, and edits now land in 31 separate
files instead of colliding in two), not build time.

An actual incremental-build win requires splitting into **separate crates**. The
module boundaries established by that split are the natural seams for it, and
that is the tracked follow-up.

### Runtime (`vm/src/runtime/`)

The bytecode execution engine.

- **`interpreter.rs`** — Main dispatch loop (~45,000 lines). Each opcode reads
  operands, manipulates the operand stack and local variables, and advances the
  program counter.

  There are **two dispatch paths**, and which one a frame gets is decided by its
  class name:

  * a *fast path* that reads opcodes straight from the raw bytecode and fuses
    common sequences (e.g. `iload_X; iload_Y; iadd`) as superinstructions; and
  * a *slow path* that calls `Instruction::decode` and matches on the decoded
    `Instruction` enum.

  The fast path uses `pop_unchecked` / `set_local_unchecked` at sites that
  assume verifier-narrow stack shapes, so it is **not** spec-correct for
  arbitrary bytecode. `class_disables_interp_fast_path`
  (`vm/src/runtime/frame.rs`) therefore excludes whole package prefixes —
  `java/`, `jdk/`, `sun/`, `com/sun/`, `org/springframework/` — with
  `java/util/*` and `Math`/`StrictMath` whitelisted back in. Two consequences
  worth knowing before you touch this file:

  1. Correctness of the fast path rests on a class-name deny-list, not on a
     property of the bytecode. The intended fix (recorded in that function's
     own TODO) is a per-method `unsafe_for_fast_path` flag computed at
     verification time.
  2. JDK and Spring bytecode runs on the slow path. The re-decode-on-every-
     execution cost that used to imply **has been fixed** — do not re-propose
     it. A one-time quickening pass pre-decodes a method's instruction stream
     into a `QuickenedCode` (`reader/src/quickened.rs`), and the slow path
     picks it up through `quickened_for_frame`
     (`vm/src/runtime/interpreter.rs:7872`), falling back to
     `Instruction::decode` only for frames with no quickened stream.

     The pc→index lookup is now O(1), including taken branches:
     `QuickenedCode` stores a bitmap of instruction starts plus a cumulative
     count per 64-byte block, and `resolve(pc)` maps any valid bytecode pc with
     two loads and a popcount. The interpreter uses this hint-free combined
     lookup directly. The remaining architectural problem is the duplicated
     opcode semantics in the fast and slow dispatch paths, not decoding or
     pc lookup.
- **`frame.rs`** — Stack frame: local variables and operand stack use
  8-byte NaN-boxed `CompactValue` slots. A parallel byte array is retained
  for the ambiguous raw `long`/`double` local cases; tags are otherwise inline.
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

Garbage collectors, extracted into the `cratonvm-gc` crate. The default is the generational collector — but see **"The default does not compact"** below before assuming the Cheney moving young gen is what runs; it is off by default and, on a JIT-warm workload, effectively unreachable. A region-based G1 collector is also present and opt-in selectable via `-XX:+UseG1GC` (experimental; Generational remains the default safety net during G1 maturation — see `docs/feature-designs/concurrent-gc-maturation.md`). ZGC is experimental and feature-gated (`--features zgc`, off by default): a metadata-only simulation plus a real STW mark-sweep heap (`ZgcRealHeap`) that is built but not yet wired into the backend dispatch (`GcBackend`), so neither is a selectable production collector.

- **`heap.rs`** — Object/array layout and allocation (semi-space).
- **`gen_heap.rs`** — Generational heap: young gen (copying) + old gen.
- **`gc.rs`** — Cheney copying collector algorithm.
- **`collector.rs`** — GC coordination and stop-the-world orchestration.
- **`card_table.rs`** — Card marking for cross-generational references.
- **`arena.rs`** — Bump-pointer memory arena.
- **`roots.rs`** — Root scanning and pointer remapping.
- **`old_gen.rs`** — Old generation management.

**The default does not compact.** Two independent gates keep the moving
(Cheney) young-gen path from running on the default configuration:

1. `moving_young` is an **opt-in** flag that defaults to **false** —
   `moving_young: present(src, "CRATONVM_MOVING_YOUNG")` at
   `types/src/flags.rs:619`, pinned by the
   `empty_source_matches_all_documented_defaults` test at `:1657`. Unless
   `CRATONVM_MOVING_YOUNG` is set in the environment, the moving cycle is
   never even requested.
2. Even when it *is* set, every cycle must carry a **per-cycle coverage
   proof** before it may relocate: `collect_garbage_inner` diverts to the
   non-moving sweep on `divert_for_incomplete_moving_coverage`, which
   `vm/src/memory/roots.rs` computes from
   `conservative_roots::refresh_moving_young_coverage_for_collection()` — every
   live compiled frame's active safepoint must certify
   `moving_young_coverage_complete`, no unregistered JIT frame may be on the
   native stack, no peer thread may be in JIT, and a frame-band verifier must
   find no young-resident word the shadow stack did not publish. Each diversion
   is counted and logged at `warn` with the specific unproven obligation
   (`gc_quiescence::incomplete_reason`), and `--verbose:gc` / `CRATONVM_GC_STATS`
   print `moving_young: cycles=N coverage_fallbacks=M` plus a per-reason
   histogram.

   (An earlier `fail_closed_non_moving = is_active() && !allow_moving_young`
   term made this unconditional — it meant `CRATONVM_MOVING_YOUNG=1` alone could
   never run a moving cycle under a live JIT frame, the only case the feature
   exists for. That term and the `CRATONVM_ALLOW_MOVING_YOUNG` flag are gone.)

So what a default `cargo build` actually gives you is a **generational
non-moving mark-sweep with selective promotion**, not a semi-space copying
young gen: no young-gen compaction, and fragmentation reclaimed only by
sweeping the free list. That is a different complexity class from the one
"Cheney moving young gen" advertises, and it is the behaviour to reason about
when reading allocation-path or GC-pause code.

Correctness is no longer what blocks the flip. `CRATONVM_MOVING_YOUNG=1` with
the JIT enabled produces the right answer and runs real moving cycles as of
2026-07-26 — the heap corruption that blocked it was five codegen sites pushing
an untagged object reference onto the JIT's simulated operand stack.
What blocks the flip now is throughput, and by less than it was: on
Binary-Trees-18 the moving path measures **2.1×** the default sweep (six
interleaved rounds, min 2905 ms vs 1363 ms), down from 5.0× before the
pre-cycle object-start walk stopped building a hash set of every object in
from-space.
bt18 is the worst case for a copying collector, and it is also the workload
where the default build cannot run at `-Xmx512m` at all while the compacting
collector completes.

**Object layout:**
```
[ObjectHeader (32 bytes)] [field0] [field1] ...
```

Field cell width depends on the field's type and on the layout in force
(`types/src/heap_types.rs`, `types/src/field_layout.rs`):

| Field kind | Width | Notes |
|------------|------:|-------|
| Reference | 8 B | `REF_FIELD_SIZE` — bare pointer, `0` = null. Default (compact reference-field layout; `CRATONVM_COMPACT_REF_FIELDS=0` opts back out to a 16 B tagged cell). |
| Primitive | 16 B | `SLOT_SIZE` — the full tagged `Value` enum: 4 B discriminant, then the payload. |

So an `int` field currently costs 16 bytes for 4 bytes of data. `CompactLayout`
already computes natural 1/2/4/8-byte offsets and a precise GC oop-map
(`ref_offsets`) for *all* field kinds, so extending the tagless representation
from references to primitives is a completion of existing work rather than a
new subsystem — see the tracked layout work.

The 32-byte header (`ObjectHeader`, `types/src/heap_types.rs`) spends 8 bytes on
an always-resident `forwarding_ptr` and 4 on an always-resident
`identity_hash_code`. Folding the forwarding pointer into the mark word would
take the header to **24 bytes**. Removing the identity hash alone saves nothing
because alignment restores four bytes of padding. Reaching 16 bytes is a
separate design project: the kind/element-type/age/flags word also has to move,
and the current inflated-monitor mark word has no spare bits.

Arrays use compact element sizes (1/2/4/8 bytes per element depending on type;
`element_byte_size`), with reference elements at `REF_ELEMENT_SIZE` = 8 B.

**Compressed oops** (`gc/src/compressed_oops.rs`) *are* wired into the live
heap now — `enable_for_live_heap` is called from `vm/src/vm/vm_init.rs:874` —
but behind an **opt-in** `-XX:+UseCompressedOops` / `CRATONVM_COMPRESSED_OOPS=1`
gate, and `use_compressed_oops` defaults to `false` (`vm/src/config.rs:426`), so
the default path does not use it. When the gate is on, reference instance fields
and reference array elements narrow from 8 bytes to 4; the klass pointer is
deliberately not narrowed (`ObjectHeader::class_id` is already a `u32`). The
reason the gate stays off is a throughput regression, not incompleteness: the
JIT's inline compact-field fast paths are disabled while compressed oops are
active, so `getfield`/`putfield` fall back to the always-correct helpers.

### JIT Compiler (`jit/` crate)

Custom x86-64 / AArch64 JIT compiler (~97,000 LoC; `x64.rs` alone is ~39,000).
Extracted into the `cratonvm-jit` crate, with shared API types in
`cratonvm-jit-api`.

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

**The IR path is materially wider than the original bring-up path, but the
two-backend split still leaks capabilities.** `ir_compatible()` now admits up
to 64 invokes, 64 instance-field operations, 64 static-field operations, and
16 allocation sites; `ir_compatible_sized` uses an 8,000-byte method cap and a
20,000-node graph cap. Static calls lower directly and virtual/interface calls
use the same inline-cache/PIC shape as the single-pass backend, default-on.

Real lowering gaps remain. The IR still declines `athrow`, `invokedynamic`,
`multianewarray`, and type checks. A surviving allocation cannot be lowered by
the IR at all: only allocations eliminated by escape analysis stay on the IR
path; otherwise compilation falls back to single-pass so the inline TLAB bump
is retained. The design goal is therefore still one shared lowering layer, but
call dispatch is no longer the blocker described by older audits.

Key optimizations: register allocation for locals, magic division,
LICM, bounds check elimination, AVX2 SIMD, on-stack replacement (OSR).
Inlining is tiered: trivial callees up to 6 bytes, cold callees up to 35 bytes,
and profile-hot callees up to 325 bytes, with total budgets of 750 bytes (cold)
or 2,000 bytes (hot). There is still no general cross-call register allocation
and no bounded-depth recursion inlining; see [BENCHMARK.md](BENCHMARK.md) for
what that costs on recursion-bound rows.

Methods are compiled after `CRATONVM_JIT_THRESHOLD` invocations (default 500;
see `vm/src/runtime/env_cache.rs`). Codegen runs **off-thread by default** — the
tiered manager enqueues a `CompilationTask` and a background worker publishes
into `shared.jit_cache`, while the mutator keeps interpreting until the entry
appears (`CRATONVM_BG_COMPILE=0` restores inline compilation). Two calling
conventions: **pure** methods (direct call) and **context** methods (receive
`SharedVm` pointer as hidden first argument).

### Native Methods (`native-builtins/`, `native-collections/`, `native-io/` crates)

A large compatibility surface of native implementations and application
bridges, split across domain-specific crates. The real-JDK default registers a
smaller bridge/intrinsic subset; the `synthetic-jdk` feature adds the synthetic
standard-library surface. The `NativeContext` trait lives in `native-api/`.

- **`native-builtins/`** — java.lang.* native methods.
- **`native-collections/`** — java.util.* native methods.
- **`native-io/`** — java.io/nio native methods.

These stubs are the **synthetic** standard library — the fallback mode, not the
default one (see Key Design Decision 1). In the default real-JDK mode the same
crates register a much smaller essential-native surface underneath real
`java.base` bytecode, so a method you find implemented here may not be the one
executing. The `NativeContext` trait (in `native-api/`) provides a VM-agnostic
interface for native methods to access the heap, class manager, and thread
state in both modes.

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

`clap`-based entry point (~6,000 LoC; `main.rs` is ~4,900). Parses arguments and
`-XX:` flags, constructs a `Vm`, calls `main(String[])`, and handles exit codes.

## Key Design Decisions

1. **Two standard libraries, and the default one is the real JDK's.** This
   decision has been reversed since it was first written, and the old wording
   ("no `JAVA_HOME`, no `rt.jar`, no dependency on any JDK installation at
   runtime") no longer describes the default build. CratonVM carries **two
   complete implementations of the Java standard library**:

   * **Real-JDK mode (the deterministic launcher default).** Class bytecode is
     loaded from an explicitly validated JDK —
     `$JAVA_HOME/jmods/java.base.jmod` or the `lib/modules` jimage, via the
     jimage reader — and Rust supplies only the essential native surface
     underneath it. `VmConfig::for_launcher()` always chooses this mode;
     detection validates availability but no longer selects the library.
   * **Synthetic mode.** A large Rust stub library stands in for `java.*`, so
     the VM can run with no JDK on the machine. It must be selected explicitly
     with `--synthetic-jdk`, and the launcher rejects that selection when the
     binary was not built with the `synthetic-jdk` Cargo feature.

   `--real-jdk` and `--synthetic-jdk` are symmetric and mutually exclusive.
   `VmConfig::default()` remains synthetic for hermetic embedding/tests, while
   the launcher default is the named `LAUNCHER_DEFAULT_JDK_MODE`. Version and
   fatal-error output identify the selected mode. `VmConfig::with_host_jdk_default()`
   is kept only as a compatibility alias for `VmConfig::for_launcher()` — see
   the doc comment on either in [`vm/src/config.rs`](vm/src/config.rs).

   Practical consequence for anyone diagnosing a failure: **establish which
   library the run used before anything else.** A missing real-JDK native and a
   synthetic-stub gap produce indistinguishable stack traces, and a fix applied
   to the wrong one changes nothing on the path that actually failed.

2. **Two-layer exception model.** `MethodCallFailed::ExceptionThrown` wraps
   Java-catchable exceptions; `MethodCallFailed::InternalError` wraps VM
   bugs. The interpreter catches the former at catch/finally blocks.

3. **Compact frame values.** Frames and operand stacks normally encode tag and
   payload together in an 8-byte `CompactValue`. Locals use a parallel
   one-byte kind only to disambiguate raw `long`/`double` patterns and preserve
   JVM category-2 slot semantics.

4. **Direct bytecode-to-x86-64, with an optional IR.** The JIT's default
   path compiles bytecode directly to machine code in a single pass, which
   keeps that path simple at the cost of limiting cross-instruction
   optimizations. Methods that qualify (`ir_compatible()`) are instead
   routed through an optional sea-of-nodes IR that enables broader
   optimization before instruction selection.

5. **Dense, memoized native dispatch.** Name resolution hashes the
   `(class, method, descriptor)` triple and verifies the full strings on every
   digest hit. The result is a stable dense `NativeMethodId`; a
   `NativeCallSite` caches the registry generation plus slot, so a warm call is
   an atomic load, generation comparison, and bounds-checked array access.
   Call sites that still use `NativeMethodRegistry::find` pay the full string
   hash and should migrate to the shared memo mechanism.

6. **Threading: one OS thread per Java thread.** `thread_start`
   (`vm/src/vm/vm_exec.rs`) spawns a real `std::thread::Builder` per
   `Thread.start()`; virtual threads are multiplexed over carriers by
   `virtual_scheduler.rs`. Beware stale comments elsewhere in the tree that
   describe Java execution as single-OS-threaded under cooperative scheduling —
   that has not been true since real thread spawning landed, and anything
   resting on it (notably the `unsafe impl Send/Sync for ObjectRef` argument in
   `types/src/value.rs`) should be read with that in mind.

7. **Configuration is env-var-driven, and the surface is large.** There are
   hundreds of distinct `CRATONVM_*` identifiers across the workspace. A
   2026-07-26 recount found 528 direct `std::env::var` / `var_os` source call
   sites in the selected core directories; `vm/src/runtime/env_cache.rs`
   contains 40 itself. The typed and grouped flag layers centralise many
   identifiers, but direct reads remain scattered. Most are
   debug or diagnostic gates, but a meaningful subset changes semantics
   (`CRATONVM_COMPACT_REF_FIELDS`, `CRATONVM_REAL_NET_SOCKETS`,
   `CRATONVM_REAL_FORKJOINPOOL`, `CRATONVM_JIT_GETFIELD_HELPER`,
   `CRATONVM_BG_COMPILE`, …). Most are cached in a `OnceLock` on first read, but
   not all — check before adding one to a hot path, and prefer extending
   `VmConfig` / `runtime::env_cache` over introducing a new bare
   `std::env::var` call.

## How to tell whether a feature actually runs

This repository has a chronic and well-evidenced failure mode: a capability
lands behind a `CRATONVM_*` environment variable, the variable defaults to
**off**, the work is recorded as "implemented", and the code never executes on
the default path. The docs then describe a VM that nobody is running. Several
of the corrections in the 2026-07-26 truth pass over this document were
instances of this.

**Confirmed instances** (all verifiable in the tree today):

| Capability | Where the default lives | Status |
|------------|-------------------------|--------|
| `moving_young` | `types/src/flags.rs::DEFAULT_MOVING_YOUNG` | The gate is now structured as opt-out with a compatibility opt-in, but the compiled default is still false. The former independent `allow_moving_young` gate has been deleted. |
| `use_compressed_oops` | `vm/src/config.rs` | Opt-in, defaults false. Fully wired but never enabled on the default path — and the doc claimed for a while that it was *not* wired, which is the same failure mode in the opposite direction. |
| `safepoint_reg_spill` | `jit/src/x64.rs:2650` | **Was** the canonical case: several call sites' own comments claimed a register spill as their protection, but that spill "only ran when the SEPARATE `CRATONVM_JIT_SAFEPOINT_REG_SPILL` env var was ALSO set — off by default, so the documented protection never actually happened." Now folded into default-on `precise_maps` and inverted to opt-**out** (`CRATONVM_NO_PRECISE_REG_SPILL`). This is the shape the fix should take. |
| `precise_maps` register-spill half | `jit/src/x64.rs:2646-2662` | Same item; `precise_maps` was default-on since 2026-07-07 while its register-spill branch was not. A flag being on does not mean all of its branches are. |

**Checklist — apply this before recording anything as "implemented":**

1. **Find the default.** Look the flag up in `types/src/flags.rs` (or
   `vm/src/runtime/env_cache.rs` / `vm/src/config.rs`). `present(src, "…")`
   means *presence is truth* — the flag is **off** unless the variable exists.
   Do not infer the default from the call site.
2. **Check that the default path takes the new branch.** Read the condition
   that guards it with the flag at its default value substituted in. If the
   branch is unreachable that way, the capability does not ship.
3. **Check for a second gate.** `moving_young` formerly needed
   `allow_moving_young` too, and `precise_maps` formerly needed
   `safepoint_reg_spill`; both duplications have since been removed. They
   remain examples of why one flag being on is not evidence that a code path
   runs.
4. **Check whether the old path was deleted or merely bypassed.** If the
   previous implementation is still present and still reachable, the new one is
   an alternative, not a replacement — and the old one is what most runs use.
5. **State the default in the sentence.** Not "X is implemented" but "X is
   implemented and is **on/off** by default (`FLAG_NAME`, `path/to/file.rs:NNN`)".
   A doc sentence that does not name the default is not a claim that can be
   checked.

**Convention for new work:** a new capability should ship **opt-out**, never
opt-in. Land it on by default with a named escape hatch for bisection
(`CRATONVM_NO_<FEATURE>`), the way `precise_reg_spill_disabled` was rewritten.
An opt-in flag is acceptable only as a temporary bring-up state, and it must be
recorded as *not shipped* until it is inverted. Correspondingly, **any doc
sentence describing a capability must name its default explicitly** — that is
the single cheapest defence against this whole class of drift.

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
