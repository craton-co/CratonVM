# cratonvm-jit-api

JIT compiler API types for CratonVM — the data surface between the VM
and its JIT back-ends.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Shared types consumed by both the VM and every JIT back-end:
`CachedBytecodeMethod` (everything needed to compile or call a method
without re-locking class metadata), `JitRuntimeHelpers` (the `#[repr(C)]`
function-pointer table the JIT embeds as absolute CALL targets), and
the optional `GpuLowering` trait seam for PTX emission. Pure types and
traits — no compiler logic lives here.

## Non-goals

- No code generation (see `cratonvm-jit` for x86-64,
  `cratonvm-jit-cuda` for PTX).
- No executable-memory allocation or W^X policy.
- Not a stable ABI for third-party JIT back-ends; the surface tracks
  the VM crate one-to-one.

## Usage

```rust
use cratonvm_jit_api::{CachedBytecodeMethod, JitRuntimeHelpers};

fn lookup_method(m: &CachedBytecodeMethod) {
    println!("compiling {}.{}{}", m.class_name, m.method_name, m.method_descriptor);
}
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
