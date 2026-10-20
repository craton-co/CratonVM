# craton-gpu4j

Build-time Java annotation packager for CratonVM GPU offload directives
(`@Parallel`, `@GpuKernel`, etc.).

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

**Build-time only, not a runtime crate.** A `build.rs` invokes `javac` + `jar`
to produce a tiny annotations JAR from the Java annotation sources
(`@Parallel`, `@GpuKernel`, etc.). Exposes two constants - `ANNOTATIONS_JAR`
and `ANNOTATIONS_DIR` - so downstream crates' build scripts can locate the
compiled output through Cargo's `DEP_CRATON_GPU_ANNOTATIONS_*` mechanism (the
`links` metadata channel).

> **The `.java` sources are not shipped in this crate.** They live in an
> external standalone Maven project (the `gpu4j` repo, formerly
> `craton-gpu-java`). See
> [Source resolution](#source-resolution) below for how `build.rs` locates them
> and how to point it at your checkout.

## Source Resolution

`build.rs` searches for the annotation source tree (a directory containing
`craton/gpu/*.java`) in this order:

1. **`$CRATON_GPU_JAVA_SRC`** - absolute path to the source directory. Set this
   to point the build at any checkout, on any OS. A value that is set but is not
   a directory is reported via `cargo:warning=` and then ignored.
2. **A sibling checkout beside the CratonVM workspace** - `gpu4j` first, then
   the legacy `craton-gpu-java`. Portable; tried on every platform.
3. **`C:/craton/gpu4j`, then `C:/craton/gpu-java`, then
   `C:/craton/craton-gpu-java`** - default install locations, consulted **only
   on Windows**. The middle one is the directory name the checkout actually has
   on the machine this was written on, which matches neither repository name.

Each candidate root is probed in every source layout the project has had,
newest first:

| Layout | Since |
| --- | --- |
| `<repo>/gpu4j-core/src/main/java` | 2026-09-06, the gpu4j rename |
| `<repo>/craton-gpu/src/main/java` | 2026-08-28, when it became a Maven aggregator |
| `<repo>/src/main/java` | before that, when the repo root was the module |

If none of these exists the build does **not** fail: it produces an empty
annotations directory, emits an empty `ANNOTATIONS_JAR`, and logs a
`cargo:warning=` explaining what was missing. Downstream consumers must
tolerate empty values.

To build the annotations on a fresh checkout, either clone the `gpu4j` repo
beside the CratonVM workspace or set the env var, e.g.:

```sh
export CRATON_GPU_JAVA_SRC=/path/to/gpu4j/gpu4j-core/src/main/java
cargo build -p cratonvm-gpu
```

## Non-goals

- No Rust runtime logic. The crate's Rust surface is just two `env!` constants
  pointing at the compiled annotations.
- No GPU code generation (see `jit-cuda`).
- No CUDA driver calls (see `cuda-bridge`).
- Not consumed by application code at runtime; only by other CratonVM crates'
  build scripts.

## Usage

```rust
// In another crate's build.rs, after declaring craton-gpu4j as a build dep:
let jar = std::env::var("DEP_CRATON_GPU_ANNOTATIONS_ANNOTATIONS_JAR")
    .expect("craton-gpu4j build script must have run first");
// pass `jar` to your javac invocation as -classpath
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
