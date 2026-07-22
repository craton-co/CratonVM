# JVM policy: real Java classes, no synthetic stubs (application-visible behavior)

This document is the **project rule** for how RustJVM should represent types and instances that real applications and the JDK expose through normal class loading. It complements (and gradually tightens) the historical "no `rt.jar`" bootstrap story: the VM must **run Java bytecode from `.class` files** whenever those classes exist on the boot or application classpath (including `java.home` discovery).

## Guiding principle

**Prefer loading real `.class` bytes** from:

- the JDK layout under `java.home` / `--java-home` (boot modules, `jrt:` / JMODs, etc.), and
- the application classpath (`--classpath`, module path, fat JARs).

Bytecode defines fields, methods, constant pools, and verification facts. Natives then **attach** to that reality instead of inventing a parallel type system.

## What we are moving away from

Do **not** introduce or rely on, for types the application or JDK already exposes as real classes:

1. **`ClassManager::ensure_synthetic_class`** and other paths that register a minimal `Class` with **`is_synthetic_stub: true`** (see [`classloading/src/class_manager.rs`](../classloading/src/class_manager.rs) — search `ensure_synthetic_class`, `is_synthetic_stub`). These stand-ins have stub methods/fields and depend on convention rather than classfiles.

2. **Heap objects allocated via `alloc_concurrent_synthetic`** (and similar helpers in `native-builtins`) **as replacements** for JDK or app types that have real classfiles. That bypasses layout and behavior encoded in bytecode and tends to diverge from HotSpot (field order, supers, reflection, etc.).

Together, "synthetic stub class + synthetic heap object" is the anti-pattern for **application-visible** API surface.

## Native code on real JDK classes (allowed, but not the same as a stub)

Registering a **native implementation for a method that also exists in a real `.class`** is allowed when it **bridges broken or incomplete dispatch** (e.g. interpreter/JIT cannot run the JDK body yet, or a known layout mismatch would misread fields). That is a **bridge**: the `Class` metadata still comes from the real classfile (`is_synthetic_stub == false`), and the native is an implementation choice for specific methods.

Contrast with a **stub**: there is **no** faithful classfile-backed `Class` for that binary name (or it was replaced by a minimal synthetic `Class`), and behavior is implied by Rust conventions and scattered registrations. Bridges are targeted; stubs are structural shortcuts.

Relevant wiring:

- [`native-builtins/src/lib.rs`](../native-builtins/src/lib.rs): **`register_essential_natives`** is used for real-JDK / "launcher" style runs; **`register_builtins`** + **`register_synthetic_overrides`** (behind the **`synthetic-jdk`** Cargo feature) aggregate the broader synthetic-JDK path. The CLI **`--synthetic-jdk`** flag forces synthetic mode even when a JDK is available (see [`vm-cli/src/main.rs`](../vm-cli/src/main.rs)).
- Compatibility shims such as [`native-builtins/src/letsgo_compat.rs`](../native-builtins/src/letsgo_compat.rs) exist partly to paper over **stub vs real** layout differences; new work should reduce reliance on that class of fix.

## Narrow exceptions

- **Array classes** — The JVM **synthesises** array types per JVMS §5.3.3; they are not loaded from `.class` files on disk. This is specified behavior, not an application-level shortcut for named JDK classes. See **`array_info`** on [`Class`](../classloading/src/class.rs) and related comments in `class_manager.rs`.
- **Test-only synthetic heaps** — e.g. helpers in [`native-builtins/src/test_utils.rs`](../native-builtins/src/test_utils.rs) for unit tests. Not a contract for production application-visible behavior.

For **named JDK and application classes** that are resolvable from the class loaders: **there is no long-term exception** that says "keep this as `is_synthetic_stub` forever." If you believe a case is purely VM-internal and invisible to application `Class.forName`, JNI, and reflection, document it in code review; default assumption is **none for app-visible API**.

## Operational hygiene (existing tooling)

To see where natives still diverge from a real classfile story, use the VM's **missing-native audit** paths (e.g. `--dump-missing-natives` / grouped dump and related flags wired through `VmConfig::audit_missing_natives` in `vm-cli`). This does not replace the policy above but helps prioritize replacing synthetic paths.

Optional **JVMTI class lifecycle hooks** (`install_class_load_hook` / `install_class_prepare_hook` in [`classloading/src/class_manager.rs`](../classloading/src/class_manager.rs)) let agents observe classes as they enter the store — useful when distinguishing real loads from synthetic bootstrap paths.

## Follow-up work

Removal and replacement of legacy synthetic paths is **deliberate follow-up**: land policy and pointers first, then shrink `ensure_synthetic_class` / `alloc_concurrent_synthetic` usage for JDK-shaped types as real classpath coverage improves.
