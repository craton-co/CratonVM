# cratonvm-classloading

Class loading subsystem for CratonVM.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Locates, loads, links, caches, and verifies Java classes. Implements the
parent-delegation classloader hierarchy (bootstrap / extension /
application), JMOD and JAR loading, classpath scanning, the
constant-pool resolution cache, JVM-spec §5.4.4 access control, and the
structural (Pass 2) and bytecode (Pass 3) verifiers with a verification-
type lattice. Holds the canonical `ClassStore` of every loaded class.

## Non-goals

- No bytecode execution (see `cratonvm-vm`).
- No JIT (see `cratonvm-jit`).
- No native method registration (see the `cratonvm-native-*` crates).

## Usage

```rust
use cratonvm_classloading::{ClassPath, ClassStore};

let class_path = ClassPath::new(&["target/classes".into(), "lib/foo.jar".into()]);
// VM threads call into the class manager to define classes, run
// <clinit>, and resolve symbolic references against the ClassStore.
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. See `LICENSE` and `NOTICE` at the workspace root.

Copyright 2024-2026 Craton Software Company.
