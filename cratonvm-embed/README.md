<!--
SPDX-License-Identifier: Apache-2.0
Copyright 2024-2026 Craton Software Company
-->

# cratonvm-embed

A curated, semver-stable **Rust** facade for embedding
[CratonVM](https://github.com/craton-co/cratonvm) — a Java Virtual Machine
implemented in Rust — into a host application.

It is a thin layer over the internal `cratonvm-vm` crate: it re-exports exactly the
supported embedding types and adds the small set of helpers an embedder always
reaches for, so a host does not have to depend on the whole VM crate and reach into
its internals. For a **C / non-Rust** host, use the
[`libcratonvm`](https://crates.io/crates/libcratonvm) crate instead.

This crate is `#![forbid(unsafe_code)]`.

## What it exposes

### Re-exported types

- `Vm`, `SharedVm`, `StackTraceFrame`
- `VmConfig`
- `Value`, `ObjectRef`, `ClassId`
- `JvmThread`, `ThreadId`
- `VmError`, `MethodCallFailed`, `MethodCallResult`

### Convenience helpers

- `make_string_array(vm, &["..."])` — build a `java.lang.String[]` (e.g. the
  `args` for a `main(String[])`).
- `read_string(vm, obj)` — read a `java.lang.String` handle back into a Rust `String`.
- `object_class_name(vm, obj)` — the runtime-class internal name of a heap object.
- `describe_failure(vm, &err)` — human-readable text for a failed call.
- `field_index(vm, class_id, name)` — resolve a named instance field to its layout
  slot index.
- `get_field_by_name(vm, obj, name)` / `set_field_by_name(vm, obj, name, value)` —
  read/write an instance field by name (resolved against the object's runtime class).

## Lifecycle

```text
Vm::new(VmConfig)        // construct + bootstrap
  -> System.initPhaseN   // advance init levels (the embedder drives these)
  -> vm.invoke(...)      // drive static/instance calls
  -> drop(vm)            // tear down
```

`SharedVm` is `Send + Sync`; the per-thread `JvmThread` inside `Vm` is not shared
across OS threads (one `JvmThread` per Java thread). Native methods are immutable
after construction — register them on the `VmConfig` / `SharedVm` before first use.
One VM per process is the only tested configuration.

## Minimal usage

```rust,no_run
use cratonvm_embed::{Vm, VmConfig, Value};

let mut vm = Vm::new(VmConfig::with_host_jdk_default());
// (drive System.initPhaseN here as the reference embedder does)
let args = cratonvm_embed::make_string_array(&mut vm, &["hello"]).unwrap();
let _ = vm.invoke(
    "HelloWorld",
    "main",
    "([Ljava/lang/String;)V",
    &[Value::Object(Some(args))],
);
```

## License

Licensed under the [Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0).

Copyright 2024-2026 Craton Software Company.
