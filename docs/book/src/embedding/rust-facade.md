# The Rust Facade (`cratonvm-embed`)

For a **Rust** host, depend on `cratonvm-embed` rather than reaching into the
internal VM crate directly. It is a thin, **curated, semver-stable facade**: it
re-exports exactly the supported types (`Vm`, `SharedVm`, `VmConfig`, `Value`,
`ObjectRef`, `ClassId`, `MethodCallFailed`, `JvmThread`, …) and adds the few
conveniences an embedder always reaches for — so your `Cargo.toml` doesn't pin
the whole internal VM surface as your compatibility contract.

```toml
[dependencies]
cratonvm-embed = "0.3"
```

By default, `cratonvm-embed` disables `cratonvm-vm` default features so the Rust
facade dependency graph stays headless and does not pull optional desktop or
experimental crates into package/readiness checks. Enable the VM-like bundle
explicitly when you need it:

```toml
[dependencies]
cratonvm-embed = { version = "0.3", features = ["vm-defaults"] }
```

`vm-defaults` mirrors the VM crate's default set exactly (`awt` + `management`).
Add `vm-experimental` on top of it for `experimental-serialization`,
`experimental-aot`, and `experimental-debug`.

You can also enable individual forwarded features such as `awt`,
`synthetic-jdk`, `management`, or `gpu-offload`.

The facade contract is the documented re-export list and helper functions.
Because `Vm`, `SharedVm`, and `JvmThread` are concrete re-exports from
`cratonvm-vm`, their public inherent methods are visible to downstream crates;
methods not documented here should be treated as lower-level VM pass-throughs
rather than the intended long-term facade surface.

The crate is `#![forbid(unsafe_code)]`: all unsafety stays behind the underlying
VM API.

## Minimal example

```rust
use cratonvm_embed::{Vm, VmConfig, Value};

let mut vm = Vm::new(VmConfig::with_host_jdk_default());
// Drive System.initPhaseN here as the reference embedder (the CLI) does.

let args = cratonvm_embed::make_string_array(&mut vm, &["hello"]).unwrap();
let _ = vm.invoke(
    "HelloWorld", "main", "([Ljava/lang/String;)V",
    &[Value::Object(Some(args))],
);
```

## Convenience helpers

The facade re-exports the VM types unchanged and adds a small set of helpers:

| Helper | Purpose |
|--------|---------|
| `make_string_array(vm, &["a", "b"])` | Build a `java.lang.String[]` with fresh, uninterned elements (e.g. for `main(String[])`). |
| `read_string(vm, obj)` | Read a `String` handle back to a Rust `String`. |
| `object_class_name(vm, obj)` | Runtime class internal name of an object. |
| `field_index` / `field_index_desc` | Resolve instance fields by name, or by name plus descriptor when disambiguation is needed. |
| `get_field_by_name` / `set_field_by_name` | Name-based instance-field access (GC-barrier correct on write). |
| `describe_failure(vm, &err)` | Human-readable text for a `MethodCallFailed`. |

## Lifecycle & threading

```text
Vm::new(VmConfig)        // construct + bootstrap to the initial init level
  → System.initPhaseN    // advance init levels (the embedder drives these)
  → vm.invoke(...)        // drive static / instance calls
  → drop(vm)              // tear down
```

- **One `JvmThread` per Java thread.** `SharedVm` is `Send + Sync`, but the
  per-thread `JvmThread` inside `Vm` is not shared across OS threads. For an
  additional Java thread, allocate its own `JvmThread` and dispatch through the
  shared-invoke free functions.
- **Natives are immutable after construction** — register any custom natives on
  the `VmConfig` / `SharedVm` before first use.
- **One VM per process** is the only tested configuration.

## Choosing a configuration

`VmConfig::with_host_jdk_default()` mirrors the CLI launcher: it boots from a
real JDK if one is detected on the host, and falls back to the synthetic
standard library otherwise (see [JDK Modes](../getting-started/jdk-modes.md)).
`VmConfig::default()` keeps synthetic mode on, which is convenient for hermetic
tests.

For the full `VmConfig` surface, the custom-native registration pattern, the
lock hierarchy, and the GC-safepoint pitfalls, consult the in-depth Rust
embedding guide in the repository; the CLI launcher is the reference embedder.
See also [Embedding Overview](overview.md) for the maturity and foreign-thread
caveats.
