# cratonvm-native-io

Java I/O native methods for CratonVM.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Native implementations backing `java.io`, `java.nio`, and `java.net`:
files, file channels (memory-mapped, `transferTo` via sendfile /
TransmitFile), non-blocking socket and server-socket channels,
asynchronous channel groups, direct byte buffers, pipes, datagram and
multicast sockets, selectors, and ZIP / JAR readers. Includes zip-bomb
guards on inflated entry size and optional CWD-confinement for embedders
running untrusted bytecode.

## Non-goals

- Not a sandbox by default. Path validation rejects `..` traversal and
  null bytes but resolves absolute paths; embedders hosting untrusted
  code must opt into `set_path_confine_to_cwd(true)`.
- No high-level Java class definitions — only the native side.
- No HTTP / TLS protocol stack beyond the underlying socket primitives.

## Usage

```rust
use cratonvm_native_api::NativeMethodRegistry;

let mut reg = NativeMethodRegistry::new();
cratonvm_native_io::register_io_natives(&mut reg);
cratonvm_native_io::set_path_confine_to_cwd(true); // multi-tenant
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
