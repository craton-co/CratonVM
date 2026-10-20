# cratonvm-types

Shared foundational types for the CratonVM project.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual
Machine implemented from scratch in Rust.

## Scope

Defines the data types every other CratonVM crate depends on: the JVM
`Value` enum, NaN-boxed `CompactValue` for stack slots, `ObjectHeader`
and `ObjectKind` for the in-heap object model, `ClassId` /
`ClassLoaderId` identifiers, the concurrent `StringPool` interner, and
`ACC_*` access-flag constants. Sits below the entire stack with no
dependencies on the rest of the workspace.

## Non-goals

- No descriptor or signature parsing (see `cratonvm-reader`).
- No allocation, GC, or heap layout policy (see `cratonvm-gc`).
- No class metadata or method tables (see `cratonvm-classloading`).

## Usage

```rust
use cratonvm_types::{ClassId, Value, StringPool};

let id = ClassId::new(42);
let v = Value::Int(7);
let name = cratonvm_types::intern("java/lang/Object");
```

## Status

Pre-1.0. API stability is best-effort. Tied to the
[CratonVM](https://github.com/craton-co/cratonvm) workspace version.

## License

Apache-2.0. The crate declares this in Cargo metadata and source files
carry SPDX headers. In a workspace checkout, see `LICENSE` and `NOTICE` at
the repository root; standalone package consumers should rely on the Cargo
license field and SPDX headers because those root files may not be adjacent
to the packaged `types/` directory.

Copyright 2024-2026 Craton Software Company.
