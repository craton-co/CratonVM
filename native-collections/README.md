# cratonvm-native-collections

Java collections native intrinsics for CratonVM.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Rust-backed native methods that accelerate `java.util.*` and
`java.util.concurrent.*` operations when the `synthetic-jdk` feature is
enabled. Real-JDK builds load these classes from JDK bytecode and execute
them in the interpreter; this crate is the synthetic alternative used by
the minimal test harness and by embedders who don't ship a JDK.

## Non-goals

- Not used in the default real-JDK build path. The `register_*` entry
  points are gated behind the `synthetic-jdk` Cargo feature.
- No I/O, no networking, no AWT — only collection containers.
- No `java.lang.*` natives (see `cratonvm-native-builtins`).

## Usage

```rust
# #[cfg(feature = "synthetic-jdk")]
# {
use cratonvm_native_api::NativeMethodRegistry;

let mut reg = NativeMethodRegistry::new();
cratonvm_native_collections::register_collections_natives(&mut reg);
# }
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
