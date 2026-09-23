# cratonvm-jit

Hand-rolled x86-64 JIT compiler for CratonVM hot methods.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Compiles self-contained JVM bytecode methods directly to native x86-64
machine code with no LLVM, no Cranelift, no external assembler.
Includes LICM, bounds-check elimination, AVX2 SIMD lowering, on-stack
replacement (OSR), a private internal calling convention (all args
through GPRs, FP-in-GPR encoding), and a precise / conservative root-
scan contract co-designed with `cratonvm-gc` so register-resident oops
remain safe across safepoints. An aarch64 back-end is in progress.

## Non-goals

- No external compiler back-end. The point of this crate is to be the
  whole codegen, not a shim over LLVM.
- No interpretation or class loading — only takes already-parsed
  bytecode and emits machine code.
- Not a general-purpose JIT framework: every emitter assumes the
  CratonVM frame layout and `JitRuntimeHelpers` ABI.

## Usage

```rust
use cratonvm_jit::JitCache;

let mut cache = JitCache::new();
// VM hands the cache a CachedBytecodeMethod; cache.compile_or_get(...)
// returns a CompiledMethod with an executable entry-point pointer.
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
