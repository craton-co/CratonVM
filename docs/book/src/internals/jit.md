# The JIT Compiler

CratonVM's JIT compiler turns hot bytecode into native machine code. It lives in
the `cratonvm-jit` crate, with shared API/IR types in `cratonvm-jit-api`. This
chapter describes its structure; [How the JIT Got
Fast](../performance/jit-internals.md) tells the optimization story, and [The
JIT Compiler (user guide)](../user-guide/jit-compiler.md) covers operating it.

## Backends

- **x86-64** is the primary, complete backend (the machine-code emitter).
- **AArch64** exists but has partial coverage. On any non-x86-64 host the JIT is
  disabled and the interpreter runs everything.

## Two compilation paths

The JIT has two ways to lower a method:

1. **Single-pass emitter.** Bytecode is lowered directly to x86-64 in one pass
   (`x64::compile_with_param_slots` → `Compiler::compile_bytecode`). This path
   is simple and compiles quickly, at the cost of limiting cross-instruction
   optimization. The eager first-call compile always uses it, and so does every
   compile the IR tier refuses.
2. **Sea-of-nodes IR tier.** The optimizing tiers (`C2` and `FullProfile`, see
   `tiered::tier_uses_optimized_backend`) and optimizing OSR entries try it,
   when `ir::ir_compatible` admits the method:

   ```text
   bytecode → IrBuilder → optimize → verify → schedule → lower (with linear-scan
   register allocation) → x86-64 → ir_evidence::accept
   ```

   This decouples optimization from instruction selection, enabling broader
   transformations. The IR body replaces the single-pass one only when
   `ir_evidence::accept` judges its transforms worth it
   (`CRATONVM_C2_ACCEPT`, default `evidence`); on any refusal the method keeps
   the single-pass body.

Both front ends use `runtime_lowering` for stateful operations whose ABI must
not drift between tiers: object allocation, the compact hashed
virtual/interface tail, and live monitor calls. The tiers differ in
optimization policy, not in those runtime semantics.

Key modules:

| Module | Responsibility |
|--------|----------------|
| `lib.rs` | JIT infrastructure: compiled-code cache, OSR entry points, helpers. |
| `x64.rs` | The x86-64 emitter, register allocation, and the LICM/BCE/SIMD analyses. |
| `aarch64.rs` | The (partial) AArch64 backend. |
| `ir.rs`, `ir_optimize.rs`, `ir_schedule.rs`, `ir_lower.rs` | The optional IR pipeline. |
| `runtime_lowering.rs` | Shared x86-64 allocation, hashed-dispatch, and monitor stubs. |

## Calling conventions

Compiled methods use one of two conventions:

- **Pure methods** — called directly.
- **Context methods** — receive a `SharedVm` pointer as a hidden first argument,
  so they can reach the heap, class manager, and thread state for slow paths
  (field/array helpers, dispatch bridges, allocation, exceptions).

Compiled code handles supported operations inline and calls back into the VM
for stateful slow paths. Virtual/interface sites use a monomorphic cache, a
four-entry PIC, and then an eight-set/two-way atomically published hashed
vtable tail before the miss helper. Allocation uses the shared TLAB-aware
helper contract, while uncontended monitors enter the thin-lock helper path.

The common native argument envelope retains up to eight decoded,
forwarded/pinned slots inline; larger signatures spill safely.

## Exceptions and deoptimization

Runtime helpers publish exception/deoptimization state out of band and return
the JIT sentinel. Call sites must check the signal because a Java `long` can
legitimately have the same bits as the sentinel.

For a protected throwing bytecode, the compiler must publish enough locals and
operand state to route the Java exception table. Unsupported handler shapes
fail closed to another tier or the interpreter. Dead locals are reconstructed
as `Undefined`, never from an uninitialized machine home.

## Optimizations

The compiler applies register-allocated locals (graph-coloring across the
callee-saved set), magic-number division, loop-invariant code motion,
bounds-check elimination (including a speculative loop-header guard), AVX2 SIMD
vectorization of reduction loops, loop unrolling, and inlined field/array
access. The full list and the order it landed in is in [How the JIT Got
Fast](../performance/jit-internals.md).

## Precise stack maps and GC safety

The collector must find object references inside compiled frames. CratonVM
records **precise JIT stack maps** — which registers and spill slots hold live
references at each safepoint — so roots in JIT frames are identified accurately.
This is on by default; the conservative-scan fallback
(`CRATONVM_NO_PRECISE_JIT_MAPS`) exists only for diagnostics.

> There is one tracked correctness item here: under the *moving* collector, a
> JIT worker's published root snapshot can lag its live spill slots at a
> stop-the-world safepoint. In practice the JIT-active path forces the
> non-moving young sweep, so it has not been observed to corrupt the heap, and a
> complete cross-thread stop-the-world JIT root scan is in progress. See
> [Security Overview](../security/overview.md).

## On-Stack Replacement (OSR)

By default, a long-running loop need not wait for its method to be re-entered:
when a loop's back-edge counter gets hot, the method is compiled and execution
transfers from the interpreter into the compiled code mid-method, with
interpreter locals copied into the JIT frame. Set `CRATONVM_JIT_OSR=0` to disable
this path for diagnosis.

## Code cache

Compiled methods are retained in a code cache. The cache can be bounded with
`CRATONVM_JIT_CODE_CACHE_MAX_MB`; when the cap is hit, further methods stay
interpreted.
