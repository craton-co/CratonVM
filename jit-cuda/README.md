# jit-cuda

Java bytecode → PTX lowering for CratonVM GPU offload.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Analyses `@Parallel` / `@GpuKernel`-annotated static Java methods to
decide GPU eligibility (see `analyzer::OffloadVerdict`), and emits a
PTX text module (`emitter::PtxModule`) the `cuda-bridge` crate can load
and launch. Tests compile real `.java` fixtures from `../test_classes/gpu/`
into `OUT_DIR/gpu-fixtures` and load those generated `.class` files rather
than synthetic bytecode arrays.

## Non-goals

- No `cudarc` or `libcuda` references. This crate produces `String` PTX;
  the runtime lives in `cuda-bridge`.
- No JVM heap access. Marshalling lives in `vm/runtime/gpu_marshal.rs`.
- No CPU JIT — `cratonvm-jit` covers x86-64.

## Usage

```rust
use cratonvm_jit_cuda::{analyze, OffloadVerdict};

let verdict = analyze(&method);
if matches!(verdict, OffloadVerdict::Eligible { .. }) {
    // hand the resolved method to the emitter::* pipeline,
    // then ship the PtxModule to cuda-bridge for load + launch.
}
```

## Fixtures and GPU-toolchain checks

`build.rs` requires `javac` on `PATH` to compile the Java fixture set. When
`javac` is missing, source discovery fails, or required fixture compilation
fails, the build emits a `cargo:warning=` and the generated fixture directory
remains empty; fixture-dependent tests then fail loudly with
`failed to read fixture` instead of passing against stale checked-in `.class`
files.

Some optional fixtures import `craton.gpu.*` runtime or annotation classes. If
the dependent `craton-gpu4j` crate does not export a Java classpath for those
classes, `build.rs` skips only those API-dependent fixtures and still compiles
the core analyzer/lowering fixtures used by default tests.

The optional `gpu-it` feature enables the `ptxas` round-trip test that is
ignored by default:

```sh
cargo test -p cratonvm-jit-cuda --features gpu-it ptxas_round_trip_vector_add
```

Set `PTXAS=/path/to/ptxas` if the NVIDIA assembler is not on `PATH`.

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
