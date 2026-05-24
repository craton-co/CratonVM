# craton-gpu

Build-time Java annotation packager for CratonVM GPU offload directives
(`@Parallel`, `@GpuKernel`, etc.).

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

**Build-time only, not a runtime crate.** Ships the Java annotation
sources under `src/main/java/craton/gpu/` and a `build.rs` that invokes
`javac` + `jar` to produce a tiny annotations JAR. Exposes two
constants — `ANNOTATIONS_JAR` and `ANNOTATIONS_DIR` — so downstream
crates' build scripts can locate the compiled output through Cargo's
`DEP_CRATON_GPU_ANNOTATIONS_*` mechanism (the `links` metadata channel).

## Non-goals

- No Rust runtime logic. The crate's Rust surface is just two `env!`
  constants pointing at the compiled annotations.
- No GPU code generation (see `jit-cuda`).
- No CUDA driver calls (see `cuda-bridge`).
- Not consumed by application code at runtime; only by other CratonVM
  crates' build scripts.

## Usage

```rust
// In another crate's build.rs, after declaring craton-gpu as a build dep:
let jar = std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR")
    .expect("craton-gpu build script must have run first");
// pass `jar` to your javac invocation as -classpath
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
