# cratonvm-native-api

Native method API for CratonVM — the `NativeContext` trait and registry.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Defines the surface that every native method crate (`native-builtins`,
`native-collections`, `native-io`, `native-awt`) targets: the
`NativeContext` trait the VM hands to each native call, the
`NativeMethodRegistry` for registration, the `FileDescriptorTable`
shared across I/O natives, and FFI / intrinsic / charset helpers.

## Non-goals

- No actual native method implementations live here (those are in the
  `native-*` sibling crates).
- No VM internals: this crate is the boundary the VM exposes outward,
  not the VM itself.

## Usage

```rust
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{Value, error::MethodCallResult};

fn my_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

let mut reg = NativeMethodRegistry::new();
reg.register("com/example/Foo", "bar", "()I", my_native);
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
