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

## Semver and feature boundary

The facade contract is the documented re-export list and helper functions. Today
`Vm`, `SharedVm`, and `JvmThread` are concrete re-exports from `cratonvm-vm`, so
their public inherent methods are visible to downstream crates; methods not
documented here should be treated as lower-level VM pass-throughs rather than the
intended long-term facade surface.

By default, `cratonvm-embed` disables `cratonvm-vm` default features. This keeps
the default dependency graph headless and avoids optional desktop or experimental
VM crates. Enable the VM-like bundle explicitly when you need it:

```toml
[dependencies]
cratonvm-embed = { version = "0.3", features = ["vm-defaults"] }
```

You can also enable individual forwarded features such as `awt`,
`synthetic-jdk`, `experimental-tls`, or `gpu-offload`.

## What it exposes

### Re-exported types

- `Vm`, `SharedVm`, `StackTraceFrame`
- `VmConfig`
- `Value`, `ObjectRef`, `ClassId`
- `JvmThread`, `ThreadId`
- `VmError`, `MethodCallFailed`, `MethodCallResult`

### Convenience helpers

- `make_string_array(vm, &["..."])` — build a `java.lang.String[]` (e.g. the
  `args` for a `main(String[])`). Elements are fresh, uninterned strings.
- `read_string(vm, obj)` — read a `java.lang.String` handle back into a Rust `String`.
- `object_class_name(vm, obj)` — the runtime-class internal name of a heap object.
- `describe_failure(vm, &err)` — human-readable text for a failed call.
- `field_index(vm, class_id, name)` — resolve a named instance field to its layout
  slot index.
- `field_index_desc(vm, class_id, name, Some(desc))` - resolve a named instance
  field with a JVM descriptor such as `"I"` or `"Ljava/lang/String;"`, useful
  when a subclass shadows a superclass field with the same name.
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

## GPU offload for embedders

The `gpu-offload` feature (listed above) forwards to `cratonvm-vm/gpu-offload` and re-exports
`VmConfig`'s four GPU fields unchanged — `gpu_offload_enabled`, `gpu_device_ordinal`,
`gpu_min_work`, `print_gpu_decisions` (all `pub`, see `vm/src/config.rs`) — so you can set them
directly on a `VmConfig` value with struct-update syntax; there is no `with_gpu_offload(...)`
builder helper here yet. Two things this crate does not do for you today:

- **Enabling `gpu-offload` alone links the stub CUDA backend, not the real driver.** Every
  device probe returns `DeviceError::NoDriver`; this mode is useful for exercising the
  `VmConfig` GPU-field plumbing without a GPU present, but it never actually dispatches work
  to a device. For the real backend, enable `gpu-driver` instead:

  ```toml
  [dependencies]
  cratonvm-embed = { version = "0.3", features = ["gpu-driver"] }
  ```

  `gpu-driver` implies `gpu-offload` and additionally turns on cuda-bridge's real CUDA Driver
  API bindings, mirroring `vm-cli`'s own composite feature
  (`gpu-driver = ["gpu", "cuda-bridge/cuda"]`, `vm-cli/Cargo.toml`). The NVIDIA driver itself is
  dlopened at runtime, not linked at build time, so building with `gpu-driver` does not require
  CUDA on the build machine — only on whichever machine runs the resulting binary with
  `gpu_offload_enabled = true`.
- **No GPU-specific convenience helpers.** This facade adds no `GpuArray`/`GpuExecutor`-style
  wrapper; you work with the four `VmConfig` fields above and the underlying `cratonvm-vm`
  offload machinery directly. The explicit-submission Java-side API is documented separately in
  `docs/gpu/async-api.md`.

See [`docs/EMBEDDING.md`](../docs/EMBEDDING.md#gpu-offload-for-embedders) for the C-ABI
comparison (GPU offload is **not** reachable from `libcratonvm` at all today) and
[`docs/known-issues/gpu-offload-followups-20260711.md`](../docs/known-issues/gpu-offload-followups-20260711.md)
for open gaps in the offload path itself.

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
