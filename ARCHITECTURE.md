# Architecture

This document describes the high-level architecture of CratonVM.
If you want to contribute, this is the place to start.

## Crate Layout

```
cratonvm/
  reader/              cratonvm-reader              .class file parser
  types/               cratonvm-types               Shared types (Value, ClassId, ObjectRef)
  native-api/          cratonvm-native-api          NativeContext trait & FD table
  native-builtins/     cratonvm-native-builtins     java.lang.* native methods
  native-collections/  cratonvm-native-collections  java.util.* native methods
  native-io/           cratonvm-native-io           java.io/nio native methods
  jit-api/             cratonvm-jit-api             JIT compiler API types
  jit/                 cratonvm-jit                 x86-64 / AArch64 JIT compiler
  classloading/        cratonvm-classloading        Class loading & bytecode verification
  gc/                  cratonvm-gc                  Garbage collectors (semi-space, G1, ZGC)
  jfr/                 cratonvm-jfr                 Java Flight Recorder
  vm/                  cratonvm-vm                  VM runtime engine
  vm-cli/              cratonvm-cli                 CLI entry point
```

**Dependency flow:**
```
vm-cli -> vm -> {classloading, gc, jit, native-builtins, native-collections, native-io, jfr}
                 -> {reader, types, native-api, jit-api}
```

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

The VM is the core of the project (~323,000+ LoC across 16 crates). It contains six
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

Garbage collectors (semi-space, G1, ZGC). Extracted into the `cratonvm-gc` crate.

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

- **`mod.rs`** — JIT infrastructure: compiled code cache, OSR entry points.
- **`x64.rs`** — x86-64 machine code emitter with 26 optimization rounds.

**Compilation pipeline:** Bytecode -> x86-64 native code (no IR).

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

4. **Direct bytecode-to-x86-64.** The JIT compiles bytecode directly to
   machine code without an intermediate representation. This keeps the
   compiler simple at the cost of limiting cross-instruction optimizations.

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
- **CI** runs clippy, fmt, tests, 65% coverage floor, Miri, and cargo-audit.
