# `Class.forName("sun.reflect.misc.Trampoline", false, null)` throws `ClassNotFoundException`

> **STATUS (2026-10-15, zs s4/rp): OPEN.** Found by the per-class probe `tools/probes/Zs4RpInner.java`
> over every `java.base` class name; not fixed in this lane (it needs the loader-identity change
> below, not a class-path change). It is outside the first 3000 names, so it does not move the
> `Zs2Fp2Meta classes 3000` checksum.

## What differs

`Zs4RpInner classes 8000 <names>` (names: `Zr7RsBoot list` on HotSpot), local JDK 25.0.3, the
2026-10-03 release binary, `--jdk-only` (default):

```text
HotSpot:  sun.reflect.misc.Trampoline | decl= | enc=- | dcl=- | nest=sun.reflect.misc.Trampoline | simple=Trampoline | flags=--- | mods=0
CVM:      sun.reflect.misc.Trampoline | forName!java.lang.ClassNotFoundException
```

HotSpot's boot loader finds `sun/reflect/misc/Trampoline.class` in `java.base` like any other
image class; `forName(.., false, null)` does not initialise it, so its `<clinit>` guard ("must not
be defined by the bootstrap classloader") never runs.

## Why

Three name rules exempt exactly this class from the bootstrap loader:

- `native-builtins/src/classloader.rs` `is_runtime_image_class_name` returns `false` for it before
  consulting the image index (so `findBootstrapClass` answers `null`);
- `native-builtins/src/classloader.rs` `is_bootstrap_class_name` and
  `native-builtins/src/lang_class.rs` `native_class_get_class_loader` (`is_jdk_pkg`) leave it out of
  the `sun/` prefix;
- `vm/src/runtime/interpreter/constants.rs` (same `sun/` exception).

They exist because `sun.reflect.misc.MethodUtil` (a `SecureClassLoader`) defines its own copy of
`Trampoline` and invokes through it (JMX `RequiredModelMBean` / `StandardMBean`); with a flat class
store, a boot-defined copy could be the one the name lookup returns to `MethodUtil`'s caller, and
the boot copy's `<clinit>` throws `Error`.

## What would close it

The bootstrap loader serves `Trampoline` like every image class, and `MethodUtil`'s define is a
second class of the same name in `MethodUtil`'s loader, which every name lookup from code defined
by that loader resolves to (HotSpot's per-loader dictionaries). That is the
"flat-store define is told from a built-in loader's own" direction
(`docs/known-issues/interpreter/i46-L5-proposal-a-flat-store-define-is-told-from-a-built-in-loaders-own-20261010.md`).
Then the three name exceptions go away (AGENTS.md: no class-name allow-lists).

Deciding rows: `Zs4RpInner classes 8000 <names>` both modes against HotSpot (the Trampoline line
SAME), plus a JMX `StandardMBean` attribute read and `MethodUtil.invoke` row (no `Error` from the
`Trampoline` `<clinit>`), both modes.
