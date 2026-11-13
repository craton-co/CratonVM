# JIT IR Optimizations — Sea-of-Nodes Pipeline and Transforms

**Status:** Shipped (default on). The optimizing tier runs on general method
shapes; individual transform passes and thresholds are configurable via runtime flags.

## What it does today

CratonVM's optimizing JIT compiler uses an SSA Sea-of-Nodes intermediate
representation (`jit/src/ir.rs`) designed for aggressive global optimization,
safe speculation, and low-overhead code generation. The optimization pipeline
in `jit/src/ir_optimize.rs` operates directly on this graph representation
in a fixed-point iteration loop.

### 1. Global Value Numbering (GVN) and Canonicalization

- **Node Deduplication:** Pure and read-only operations with identical operands
  are canonicalized to a single representative node via hash-consing.
- **Algebraic Simplification:** Identity transformations (e.g., `x + 0 -> x`,
  `x * 1 -> x`, `x & 0 -> 0`, `x ^ x -> 0`), constant folding of unary and binary
  arithmetic, and strength reduction (replacing multiplications/divisions by constants
  with shifts and adds).
- **Branch Simplification:** Constant condition elimination and dead control path
  pruning.

### 2. Memory Optimizations and Dead Store Elimination (DSE)

- **Memory Dependency Graph:** Memory states are tracked explicitly through memory
  effect edges connecting loads, stores, calls, and allocation nodes (`Op::Load`,
  `Op::Store`, `Op::New`, `Op::NewArray`, `Op::Call`).
- **Redundant Load Elimination:** Consecutive loads from identical base and offset
  locations without intervening aliasing stores forward the previously loaded value directly.
- **Dead Store Elimination (DSE):** Stores overwritten prior to any observable read or
  safepoint exit are eliminated (`eliminate_dead_stores`).
- **Write-Only Store Elimination:** Write-only stores to non-escaping newly allocated
  objects are eliminated when the allocation itself is elided.

### 3. Loop Optimizations: LICM and Unrolling

- **Loop Invariant Code Motion (LICM):** Computations inside loop bodies whose inputs
  are loop-invariant are hoisted into loop preheaders (`CRATONVM_JIT_LICM=1` by default).
- **Counted Loop Unrolling:** Small loops with statically known trip counts and simple
  induction variable progressions are unrolled completely or partially to eliminate loop
  control overhead and expose intra-iteration instruction scheduling opportunities
  (`CRATONVM_JIT_UNROLL=1` by default).

### 4. Escape Analysis and Scalar Replacement

- **Connection Graph / Escape State:** Object allocations (`Op::New`, `Op::NewArray`)
  are tracked to classify escape status into `NoEscape`, `ArgEscape`, or `GlobalEscape`.
- **Scalar Replacement:** For allocations classified as `NoEscape`, heap allocation is
  eliminated. Object fields and array elements are scalarized into virtual SSA values
  or local register slots, avoiding garbage collection allocation pressure.
- **Deoptimization Support:** When deoptimization occurs within a method where objects
  were scalarized, the VM's deopt subsystem reconstructs heap representations of the
  virtual objects dynamically before resuming execution in the interpreter.

### 5. Atomics and Concurrency Primitives

- **Memory Ordering Lowering:** VarHandle and Unsafe atomic operations (e.g., CAS,
  `getAndAdd`, `getAndSet`, volatile loads/stores) are recognized as first-class IR operations
  with appropriate memory barrier semantics (acquire, release, sequentially consistent).
- **Atomic Operations Coalescing:** Redundant memory barriers and fences around loop-local
  or non-volatile accesses are cleaned up during the memory optimization phase.

### 6. Devirtualization and Inlining

- **Class Hierarchy Analysis (CHA):** Final methods, private methods, and single-implementor
  interface methods are resolved at compile time for direct call dispatch.
- **Polymorphic Inline Caches (PIC):** Monomorphic and bimorphic call sites observe class
  tags and inline hot target bodies protected by fast class-id guard checks.
- **IR Splicing:** Inlined method bodies are integrated directly into the caller's Sea-of-Nodes
  graph, enabling cross-method GVN, constant propagation, and dead code elimination.

## Flags and Configuration

Defaults are read dynamically from runtime configuration and environment:

| Flag | Default | Description |
|---|---|---|
| `CRATONVM_JIT_IR_CALL` | `1` | Enable inlining and IR call operations |
| `CRATONVM_JIT_LICM` | `1` | Loop invariant code motion |
| `CRATONVM_JIT_UNROLL` | `1` | Counted loop unrolling |
| `CRATONVM_JIT_SCALAR_NEW` | `1` | Scalar replacement of `new` objects |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT` | unset | Kill switch for scalar replacement |
| `CRATONVM_NO_IR_BRANCHY` | unset | Fallback to linear code generation for branchy control flow |

## Residual Gaps and Future Work

- **Loop Vectorization:** Auto-vectorization of counted numeric loops using AVX2/AVX-512
  is planned behind architecture-specific instruction selection.
- **Interprocedural Escape Analysis:** Extending escape analysis across non-inlined method
  boundaries using summary annotations.
