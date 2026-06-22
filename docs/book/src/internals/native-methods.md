# Native Methods

CratonVM implements the parts of the Java standard library that a normal JVM
implements in C (the `native` methods) as Rust functions, split across
domain-specific crates. This is what lets CratonVM run without `rt.jar`, and —
in synthetic mode — without any JDK at all.

## The crates

| Crate | Covers |
|-------|--------|
| `cratonvm-native-builtins` | `java.lang.*`, plus security/crypto, reflection, and related. |
| `cratonvm-native-collections` | `java.util.*` collections. |
| `cratonvm-native-io` | `java.io.*` and `java.nio.*`. |
| `cratonvm-native-awt` | AWT/Swing/Java2D native peers (headless). |
| `cratonvm-native-api` | The `NativeContext` trait and FD table that the above depend on. |

Together they register **thousands of native methods**. See [Standard Library
Coverage](../java-support/standard-library.md) for how to generate an exact
catalog.

## The `NativeContext` trait

Native methods are VM-agnostic: they receive a `NativeContext` that exposes the
operations they need without depending on the VM's internals directly:

- `alloc_object(class_id)` — allocate a new object.
- `get_field(obj, index)` / `set_field(obj, index, value)` — instance-field
  access (writes are GC-barrier correct).
- `get_string_value(obj)` — extract a Rust `String` from a Java `String`.
- `create_string(s)` — create a Java `String` from a Rust `&str`.
- `throw_exception(class, message)` — raise a Java exception.

A native method has a signature of roughly `(ctx, args) -> Result<Option<Value>>`:
`args[0]` is the receiver for instance methods, with parameters following.

## Dispatch

Natives are looked up by **hashing** `"class.method:descriptor"`, giving O(1)
dispatch. Collisions are detected at registration time, so two natives can't
silently shadow each other by accident.

Each registered native is classified by kind:

- **Intrinsic** — a fast inline-cache shortcut for a hot, well-known method.
- **Bridge** — connects to real JDK bytecode behavior.
- **Synthetic stub** — a standalone Rust implementation used when there's no real
  JDK class to run.

You can dump the full classified registry at runtime with
`--dump-native-registry`, and audit which natives a program *needs but lacks*
with `--XX:AuditMissingNatives` — see [Debugging &
Diagnostics](../user-guide/debugging.md).

## Real-JDK vs. synthetic

In real-JDK mode, standard-library *classes* run their real bytecode and call
down into these natives only for genuinely native operations. In synthetic mode,
the synthetic-stub natives implement the classes themselves. A set of
`CRATONVM_REAL_*` / `CRATONVM_SYNTHETIC_*` switches let you pick the path
per-subsystem for differential testing — see
[Configuration](../user-guide/configuration.md). The project's direction is
bytecode-first: prefer running real `.class` files for application-visible types
over synthetic stubs.

## Adding a native method

The contribution workflow for adding a native — choosing the crate, registering
the method, and testing it — is in the [Contributing
Guide](../contributing/contributing.md#adding-a-native-method).
