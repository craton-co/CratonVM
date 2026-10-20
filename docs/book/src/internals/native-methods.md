# Native Methods

CratonVM implements the parts of the Java standard library that a normal JVM
implements in C (the `native` methods) as Rust functions, split across
domain-specific crates. This is what lets CratonVM run without `rt.jar`, and —
in synthetic mode — without any JDK at all.

## The crates

| Crate | Covers |
|-------|--------|
| `cratonvm-native-builtins` | `java.lang.*`, registration, reflection, Java-object marshalling, and related bridges. |
| `cratonvm-native-builtins-crypto` | Separately compiled cryptographic compatibility kernels. |
| `cratonvm-native-builtins-security` | Separately compiled JDK security and SunEC implementation pack. |
| `cratonvm-native-collections` | `java.util.*` collections. |
| `cratonvm-native-io` | `java.io.*` and `java.nio.*`. |
| `cratonvm-native-awt` | AWT/Swing/Java2D bridge natives for headless peers and in-memory rendering. |
| `cratonvm-native-api` | The `NativeContext` trait and FD table that the above depend on. |

Together they register **thousands of native methods**. See [Standard Library
Coverage](../java-support/standard-library.md) for how to generate an exact
catalog.

## The `NativeContext` capability facade

Native methods are VM-agnostic: they receive a `NativeContext` composed from
narrow capability traits rather than depending on the VM's internals directly.
The capability families include class, invoke, heap, thread, exception, GPU,
and system access. Common operations include:

- `alloc_object(class_id)` — allocate a new object.
- `get_field(obj, index)` / `set_field(obj, index, value)` — instance-field
  access (writes are GC-barrier correct).
- `get_string_value(obj)` — extract a Rust `String` from a Java `String`.
- `create_string(s)` — create a Java `String` from a Rust `&str`.
- `throw_exception(class, message)` — raise a Java exception.

A native method has a signature of roughly `(ctx, args) -> Result<Option<Value>>`:
`args[0]` is the receiver for instance methods, with parameters following.

Loader-aware invoke operations preserve the declaring class/loader identity;
native code must not replace them with a global lookup by binary class name.

The JIT/native bridge uses an eight-slot inline scratch contract for ordinary
x86-64 argument decoding, forwarding, and pin-index preparation. Descriptors
larger than that use a correct heap-backed fallback; eight is a fast-path
capacity, not an API limit.

The crypto/security split is physical. Cargo compiles those kernels as
independent crates; `cratonvm-native-builtins` retains the registry and
Java-object conversion boundary.

## Dispatch

Natives are looked up by **hashing** `"class.method:descriptor"`, giving O(1)
dispatch. Collisions are detected at registration time, so two natives can't
silently shadow each other by accident.

Each registered native is classified by kind:

- **Intrinsic** — a fast inline-cache shortcut for a hot, well-known method.
- **Bridge** — connects to real JDK bytecode behavior.
- **Synthetic stub** — a standalone Rust implementation used when there's no real
  JDK class to run.

Registrations deliberately **overwrite by triple**: a later
`(class, name, descriptor)` registration replaces an earlier one in place, and
the last write wins. That is how subsystem-specific passes refine the boot
registrations, and until schema 2 the replaced entry left no trace at all.

You can dump the full classified registry at runtime with
`--dump-native-registry`, and audit which natives a program *needs but lacks*
with `--XX:AuditMissingNatives` — see [Debugging &
Diagnostics](../user-guide/debugging.md).

### The native registry census (schema 2)

`--dump-native-registry <FILE>` writes `"schema_version": 2`. Schema 1 carried
only `{class, name, descriptor, kind}` — enough to *count* stubs, not enough to
retire any. Schema 2 adds the provenance that makes a row actionable:

```json
{
  "schema_version": 2,
  "counts": { "bridge": 0, "intrinsic": 0, "synthetic-stub": 0, "total": 0 },
  "natives": [
    {
      "class": "java/lang/Object",
      "name": "hashCode",
      "descriptor": "()I",
      "kind": "intrinsic",
      "registered_by": "<redacted>/lib.rs:1234",
      "overwrote": null,
      "invocations": 0,
      "real_declaring_method": null
    }
  ]
}
```

| Field | Meaning |
|-------|---------|
| `kind` | `intrinsic`, `bridge` or `synthetic-stub` — the `NativeKind` tag. |
| `registered_by` | The `register()` **call site**, `file:line`, captured with `#[track_caller]` rather than a string built at registration time. It names the `register_*` pass that produced the entry. `null` if no site was recorded. |
| `overwrote` | The `NativeKind` this registration replaced in place, or `null` when it replaced nothing. This is the supersession history that was previously unrecoverable. |
| `invocations` | Runtime **dispatches** this run, not registrations. `0` means removing the entry would cost this workload nothing. |
| `real_declaring_method` | Reserved. Currently emitted as `null` on every row — see below. |

`counts` is seeded with all three kinds, so a stub-free census still emits
`"synthetic-stub": 0` and a gate can assert on the key's value rather than on
its absence. Rows are sorted by `(class, name, descriptor, registered_by)` so
the file is byte-stable and can be committed as a baseline; the registration
order the registry returns internally is deliberately *not* stable across
builds.

Two caveats to read the columns correctly:

- **`invocations` is a lower bound.** It is incremented on the dispatch paths
  that hold a native slot handle (the interpreter's invoke paths and
  `vm_exec`). The warm cached virtual-native path, the JIT's native thunk and
  the paths that resolve a native by name only carry no slot handle, so their
  calls are not counted. A `0` is evidence, not proof.
- **`real_declaring_method` is a known, deliberate gap.** The intended object
  answers "does the real JDK image declare this exact triple, is it
  `ACC_NATIVE`, does it carry a `Code` attribute" — the fact that separates a
  legitimate bridge from a stub shadowing real bytecode. Answering it needs a
  *non-initiating* lookup against the boot image; resolving it through the
  ordinary class-loading path at shutdown would load hundreds of classes the
  run never touched and change what the census reports about itself. The key
  is emitted with a `null` value rather than invented, so a consumer can tell
  "not answerable yet" from "schema changed".

`registered_by` paths are redacted to `<redacted>/<basename>:<line>` unless
`--explain-jdk-only` is passed, so a census stays diffable across hosts and
carries no developer's home directory into a bug report.

`cratonvm-native-awt` registrations are classified as **Bridge** natives:
they satisfy native entry points reached by real JDK AWT/Swing/Java2D classes.
That category does not imply full desktop readiness; current support is
headless and does not instantiate OS windows.

## Real-JDK vs. synthetic

In real-JDK mode, standard-library *classes* run their real bytecode and call
down into these natives only for genuinely native operations. In synthetic mode,
the synthetic-stub natives implement the classes themselves. A set of
`CRATONVM_REAL_*` / `CRATONVM_SYNTHETIC_*` switches let you pick the path
per-subsystem for differential testing — see
[Configuration](../user-guide/configuration.md). The project's direction is
bytecode-first: prefer running real `.class` files for application-visible types
over synthetic stubs.

`--jdk-only` is that direction expressed as a runtime policy: under it a
`SyntheticStub` may be neither registered nor invoked, while reviewed
intrinsics and bridges remain allowed. See [JDK-Only
Mode](../user-guide/jdk-only-mode.md).

## Adding a native method

The contribution workflow for adding a native — choosing the crate, registering
the method, and testing it — is in the [Contributing
Guide](../contributing/contributing.md#adding-a-native-method).
