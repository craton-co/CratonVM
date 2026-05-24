# cratonvm-vm

The CratonVM Java Virtual Machine core: interpreter, threading,
synchronization, exception handling, and JIT / GC bridge.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Ties the rest of the workspace together into a running JVM. Owns the
bytecode interpreter (140+ fast-path opcodes), the call-stack and frame
model, per-thread state (`JvmThread`, `SharedVm`), monitor / lock
inflation, exception unwind tables, the live object model, the JIT
dispatch glue, native method invocation, virtual-thread scheduling,
class-initialization (`<clinit>`) ordering, and the `Vm` orchestrator
that boots a JDK image and runs `main(String[])`.

## Non-goals

- No class-file parsing (delegated to `cratonvm-reader`).
- No class loading / verification (delegated to `cratonvm-classloading`).
- No code generation (delegated to `cratonvm-jit` / `jit-cuda`).
- No CLI argument handling (delegated to `cratonvm-cli`).

## Usage

```rust
use cratonvm_vm::{Vm, VmConfig};

let mut vm = Vm::new(VmConfig::default());
// Use cratonvm_vm::vm::invoke_on_class_shared to dispatch into a
// loaded Java method; the `cratonvm-cli` crate is the canonical
// embedder for full `main(String[])` boot.
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
