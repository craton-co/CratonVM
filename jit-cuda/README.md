# jit-cuda

Java bytecode → PTX lowering for CratonVM GPU offload.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Analyses `@Parallel` / `@GpuKernel`-annotated static Java methods to
decide GPU eligibility (see `analyzer::OffloadVerdict`), and emits a
PTX text module (`emitter::PtxModule`) the `cuda-bridge` crate can load
and launch. Tests load real `.class` files from `test_classes/gpu/`
rather than synthetic bytecode arrays.

## Non-goals

- No `cudarc` or `libcuda` references. This crate produces `String` PTX;
  the runtime lives in `cuda-bridge`.
- No JVM heap access. Marshalling lives in `vm/runtime/gpu_marshal.rs`.
- No CPU JIT — `cratonvm-jit` covers x86-64.

## Usage

```rust
use jit_cuda::{analyze, OffloadVerdict};

let verdict = analyze(&method);
if matches!(verdict, OffloadVerdict::Eligible { .. }) {
    // hand the resolved method to the emitter::* pipeline,
    // then ship the PtxModule to cuda-bridge for load + launch.
}
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
