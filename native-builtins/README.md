# cratonvm-native-builtins

Java core native methods for CratonVM (`java.lang.*`, `java.security.*`,
`javax.crypto.*`, `java.lang.invoke.*`).

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Implements the essential native surface the JDK requires to boot:
`Object`, `Class`, `System`, `Thread`, `Throwable`, `String`, `Math`,
the unsafe / VarHandle / MethodHandle plumbing, `MessageDigest` and
`Signature` JCA hooks, security providers, and framework shims (SLF4J
binder stubs, Spring-Boot Logback initialization, reflective proxy
generation). Default-on `legacy-synthetic-crypto` feature exposes
provider stubs; modern algorithms (ML-KEM, ML-DSA, HKDF) deliberately
reject with `NoSuchAlgorithmException`.

## Non-goals

- No I/O, networking, or file natives (see `cratonvm-native-io`).
- No collections (see `cratonvm-native-collections`).
- No AWT / Swing / Java2D (see `cratonvm-native-awt`).
- No real post-quantum cryptography — stubs only.

## Usage

```rust
use cratonvm_native_api::NativeMethodRegistry;

let mut reg = NativeMethodRegistry::new();
cratonvm_native_builtins::register_essential_natives(&mut reg);
cratonvm_native_builtins::register_builtins(&mut reg);
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
