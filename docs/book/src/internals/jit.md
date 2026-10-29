# The JIT Compiler

CratonVM's Just-In-Time (JIT) compiler compiles hot Java bytecode into optimized native machine code. It is implemented in the `cratonvm-jit` crate, with shared API and Intermediate Representation (IR) definitions in `cratonvm-jit-api`.

For user-facing runtime control and tuning, see [The JIT Compiler (User Guide)](../user-guide/jit-compiler.md). For optimization details, see [JIT Optimization Internals](../performance/jit-internals.md).

## Backends

- **x86-64**: The primary, fully featured native backend, generating direct machine code with AVX2 vectorization, graph-coloring register allocation, and precise stack maps.
- **AArch64**: Secondary backend under active development. On non-x86-64 platforms where the backend is not enabled, CratonVM executes code through its fast bytecode interpreter.

## Tiered Compilation Architecture

CratonVM utilizes a multi-tiered compilation pipeline:

```text
Bytecode Interpreter ──> Profiling / Invocation Counters
                             │
                             ├──> Tier 1: Single-Pass Baseline Emitter
                             │
                             └──> Tier 2: SSA Sea-of-Nodes Optimizing IR Pipeline
```

### 1. Single-Pass Emitter (Tier 1)
Bytecode is lowered directly into x86-64 instructions in a fast single pass (`x64::compile_with_param_slots` → `Compiler::compile_bytecode`). This tier provides rapid compilation with minimal memory overhead, immediately accelerating warm methods and serving as a reliable fallback when methods do not qualify for higher-level IR transformations.

### 2. Sea-of-Nodes Optimizing IR Pipeline (Tier 2)
Hot methods and OSR loop targets enter the optimizing tier:

```text
Bytecode → IrBuilder → IR Optimizer (Escape Analysis, LICM, BCE) → Verify → Schedule → Linear-Scan / Graph-Coloring Regalloc → x86-64 Lowering
```

The IR decouples high-level transformations from target instruction selection, enabling aggressive whole-method optimizations.

## Core Optimization Passes

The JIT compiler incorporates a comprehensive suite of optimizations:

- **Escape Analysis & Scalar Replacement**: Traces object allocation escapes. Non-escaping objects are dismantled and replaced by scalar variables in registers or stack locations, removing heap allocation and garbage collection overhead.
- **Loop-Invariant Code Motion (LICM)**: Hoists invariant array accesses, field reads, and expressions out of loop bodies into preheaders.
- **Bounds-Check Elimination (BCE)**: Performs inductive range analysis on array indexing and inserts speculative loop-header guards, eliminating per-iteration array bounds checks in hot loops.
- **AVX2 SIMD Auto-Vectorization**: Detects reduction loops and data-parallel iterations, vectorizing operations using 256-bit AVX2 SIMD instructions with clean scalar loop remainders.
- **Global Register Allocation**: Employs graph-coloring and linear-scan register allocation across callee-saved and scratch registers to minimize memory traffic and stack frame spilling.
- **Magic-Number Division**: Converts integer division and modulo operations with constant divisors into multiply-and-shift sequences, avoiding costly hardware `IDIV` instructions.
- **Polymorphic Inline Caching (PIC)**: Dispatches interface and virtual calls through monomorphic caches, 4-entry inline caches, and an 8-set 2-way hashed vtable tail.

## On-Stack Replacement (OSR)

Long-running loops trigger On-Stack Replacement automatically. When a loop back-edge counter exceeds the hotness threshold, the JIT compiles the active loop and reconstructs the execution state:
- Interpreter local variables and operand stack entries are transferred into corresponding slots in the compiled native frame.
- Execution transfers seamlessly mid-method into native code without waiting for the method to return.
- Diagnostics toggle: `CRATONVM_JIT_OSR=0` disables OSR for troubleshooting.

## Precise Stack Maps and GC Safety

To ensure garbage collection correctness, the compiler emits **precise JIT stack maps** at every safepoint. These maps explicitly record which CPU registers and stack slots hold live object references. 

During safepoint pauses, root scanners (including ZGC and the Generational collector) inspect the precise stack maps of active compiled frames, accurately tracking references and updating them when compaction moves objects. Precise stack maps are enabled by default; `CRATONVM_NO_PRECISE_JIT_MAPS=1` is reserved strictly for diagnostic bisection.

## Calling Conventions and Runtime Helpers

Compiled methods adhere to optimized native calling conventions:
- **Pure Methods**: Arithmetic and purely computational leaf methods are invoked directly via native register-passing conventions.
- **Context Methods**: Stateful methods receive a `SharedVm` context pointer as an implicit parameter, giving compiled code fast access to TLAB allocation, class loaders, exception handling, and monitor tables.
- **Deoptimization & Exceptions**: If an unexpected type check fails or an exception is thrown, the compiled method cleanly unwinds, reconstructs interpreter frames, and routes to Java exception handlers.

## Code Cache Management

Compiled machine code resides in an executable memory code cache. The cache ceiling is governed by `CRATONVM_JIT_CODE_CACHE_MAX_MB`. When full, existing compiled methods continue executing while newly invoked methods fall back to interpreter execution.
