# cratonvm-native-collections

Java collections native intrinsics for CratonVM.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Rust-backed native methods for selected `java.util.*` and
`java.util.concurrent.*` operations. Most registrations are bridge
intrinsics that complement real-JDK bytecode; the `synthetic-jdk` feature
keeps additional synthetic-stub-only registrations available for the
minimal test harness and embedders that do not ship a full JDK.

## Non-goals

- Not a replacement for the full JDK collections implementation. Methods
  without native registrations continue to run through real-JDK bytecode in
  the default VM.
- Synthetic-stub-only registrations remain behind the `synthetic-jdk`
  Cargo feature.
- No I/O, no networking, no AWT — only collection containers.
- No `java.lang.*` natives (see `cratonvm-native-builtins`).

## Usage

```rust
use cratonvm_native_api::NativeMethodRegistry;

let mut reg = NativeMethodRegistry::new();
cratonvm_native_collections::register_collections_natives(&mut reg);
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
