<!--
SPDX-License-Identifier: Apache-2.0
Copyright 2024-2026 Craton Software Company
-->

# libcratonvm

C-ABI embedding library for [CratonVM](https://github.com/craton-co/cratonvm) — a
Java Virtual Machine implemented in Rust. This crate lets a **C / C++ / non-Rust**
host process create and drive a CratonVM JVM. For a **Rust** host, use the
[`cratonvm-embed`](https://crates.io/crates/cratonvm-embed) facade instead.

The crate builds as a `cdylib` (`libcratonvm.so` / `cratonvm.dll`), a `staticlib`
(`libcratonvm.a` / `cratonvm.lib`), and an `rlib`.

## What it exposes

The library presents two C-ABI surfaces over the same VM machinery:

### 1. JNI Invocation API

The three standard `jni.h` bootstrap entry points a host calls when it loads a
`libjvm` substitute:

- `JNI_CreateJavaVM(JavaVM **pvm, void **penv, void *args)`
- `JNI_GetDefaultJavaVMInitArgs(void *args)`
- `JNI_GetCreatedJavaVMs(JavaVM **vmBuf, jsize bufLen, jsize *nVMs)`

A successful `JNI_CreateJavaVM` hands back a `JavaVM*` (invocation table) and a
`JNIEnv*` (the standard JNIEnv function table: `FindClass`, `GetStaticMethodID`,
`CallStaticVoidMethod`, …). Mirroring HotSpot, **at most one VM per process** is
allowed; a second create returns `JNI_EEXIST`. The init args accept the common
HotSpot option forms: `-Xmx<size>`, `-cp` / `-classpath` / `--class-path`, and
`-D<key>=<value>`, plus the launcher's own `--real-jdk` / `--synthetic-jdk` /
`--jdk-only` (see [Compatibility mode](#compatibility-mode)). Unsupported JNI
versions fail with `JNI_EVERSION`; unknown or malformed options fail with
`JNI_ERR` unless `ignoreUnrecognized` is non-zero. A configuration conflict is a
bare `JNI_ERR` with no message — `JNI_CreateJavaVM` has nowhere to put one; use
the flat API when you want the diagnostic text.

### 2. Flat opaque-handle C API (`cratonvm_*`)

A curated convenience surface over opaque handles + POD, for hosts that prefer
not to drive the raw JNIEnv table by slot index:

- **Lifecycle:** `cratonvm_create`, `cratonvm_create_with_compatibility`,
  `cratonvm_destroy`
- **References:** `cratonvm_release_ref`
- **Classes:** `cratonvm_load_class`, `cratonvm_class_name`, `cratonvm_object_class`
- **Invocation:** `cratonvm_invoke_static`, `cratonvm_invoke_virtual`
- **Strings:** `cratonvm_new_string`, `cratonvm_string_utf8`, `cratonvm_free_string`
- **Fields:** `cratonvm_field_count`, `cratonvm_field_index`, `cratonvm_field_index_desc`,
  `cratonvm_get_field`, `cratonvm_get_field_by_name`, `cratonvm_set_field`,
  `cratonvm_set_field_by_name`
- **Policy:** `cratonvm_compatibility_mode`, `cratonvm_compatibility_mode_supported`
- **Errors:** `cratonvm_last_error`, `cratonvm_clear_error` (thread-local, mirroring
  JNI's per-thread pending exception)

Values are exchanged as `CratonValue` — a `#[repr(C)]` tagged POD (`tag` + 8-byte
`payload`) covering `int`/`long`/`float`/`double`/`object`. Method arguments are
passed as a typed `CratonValue` array plus a count (not C varargs).

Only one VM surface may be active in a process at a time: either one
Invocation-API VM or one flat `CratonVm`. `cratonvm_create` returns `NULL` with
`cratonvm_last_error()` set if another VM surface is active. `CratonRef` values
are opaque object tokens, not heap addresses; stale/fabricated object tokens and
unknown inbound `CratonValue` tags fail the call instead of being treated as
`null`. Each nonzero `CratonRef` returned by the API pins a JNI global ref until
the host calls `cratonvm_release_ref` for that returned token or destroys the VM.

Every exported `extern "C"` entry point wraps its body in `catch_unwind`, so a Rust
panic never unwinds across the C boundary.

## Compatibility mode

*Compatibility mode* selects **which substitutions the VM may make**. It is
orthogonal to the JDK mode (`--real-jdk` / `--synthetic-jdk`), which selects
**which class library boots**; both surfaces set the two independently.

| C constant | Value | Meaning |
|---|---|---|
| `CRATONVM_COMPATIBILITY_COMPATIBLE` | `0` | Today's behaviour: bridges, intrinsics **and** compatibility shims. The default on every entry point. |
| `CRATONVM_COMPATIBILITY_JDK_ONLY` | `1` | Real JDK class bytes are authoritative: no class fabricated without real bytes, no synthetic-stub native registered or invoked. Requires a real JDK runtime image. |

The numeric values are a published, stable, **append-only** ABI — deliberately
`cratonvm_jint` rather than `cratonvm_jboolean`, so a third enforcement posture
can be added later without breaking a compiled host.

Two routes reach strict mode, and both are explicit:

```c
/* 1. Typed argument (flat API). A C host has no command line, and
 *    JavaVMInitArgs is often assembled far from the call site. */
if (cratonvm_compatibility_mode_supported(CRATONVM_COMPATIBILITY_JDK_ONLY) != 1) { ... }
CratonVm *vm = cratonvm_create_with_compatibility(&args, CRATONVM_COMPATIBILITY_JDK_ONLY);
printf("mode = %d\n", (int)cratonvm_compatibility_mode(vm));   /* read back: 1 */

/* 2. Option string, spelled exactly as the launcher flag — the ONLY route for
 *    JNI_CreateJavaVM(), which takes nothing but a JavaVMInitArgs. */
JavaVMOption opts[1] = { { (char *)"--jdk-only", NULL } };
```

Rules that are easy to get wrong:

- **The default is `COMPATIBLE`, and you reach it by doing nothing.** Strict mode
  is never inferred from a Cargo feature, from `CRATONVM_REAL` /
  `CRATONVM_NO_STUBS`, or from what the host machine has installed — those select
  a native-registry filter only and cannot express the class-loading or dispatch
  half of the contract.
- **The JDK mode is asymmetric between entry points; the compatibility mode is
  not, and that is deliberate.** This crate builds on the launcher's base config
  (`VmConfig::with_host_jdk_default()`, real JDK) while `VmConfig::default()` —
  the in-process Rust embedding path — is synthetic, because *which class library
  loads* is a hermeticity question. *Which substitutions are permitted* is not:
  strict mode rejects work that compatible mode accepts, so a host that has not
  asked for it must never be handed it. Do not "align" the second split with the
  first.
- **An unknown ABI integer is rejected, never clamped to `COMPATIBLE`** — a host
  compiled against a newer header and run against an older library is told so.
- **`CRATONVM_COMPATIBILITY_COMPATIBLE` together with `--jdk-only` is a
  contradiction error, not a precedence rule.** Both are explicit statements of
  policy; picking a winner would run a host that assembled its options from two
  places under a policy neither half asked for.
- **Compatibility is validated before the JDK-availability check**, so
  `--jdk-only --synthetic-jdk` is reported as the flag conflict it is rather than
  as a missing JDK.

A rejected request returns `NULL` from `cratonvm_create*` with
`cratonvm_last_error(NULL)` holding `"<entry_point>: invalid configuration: …"` —
verbatim the text the `cratonvm` launcher prints. `JNI_CreateJavaVM` returns a
bare `JNI_ERR` and sets no message; use the flat API when you want the
diagnostic text.

JDK-only is an internal diagnostic, not a production posture: it is at stage 1 of
4 of its rollout, so expect failures on programs that run fine under
`CRATONVM_COMPATIBILITY_COMPATIBLE`. See
[`docs/EMBEDDING.md`](../docs/EMBEDDING.md#choosing-a-compatibility-mode) and
[`docs/feature-designs/jdk-only-mode.md`](../docs/feature-designs/jdk-only-mode.md).

## Minimal usage (C, via the flat API)

```c
#include "cratonvm.h"   /* generated via cbindgen; see build.rs */

CratonVm *vm = cratonvm_create(NULL);   /* NULL args -> defaults */
if (!vm) { /* cratonvm_last_error(NULL) has the message */ return 1; }

/* int Integer.parseInt(String) */
CratonRef s = cratonvm_new_string(vm, "42");
CratonValue arg = { .tag = 5 /* OBJECT */, .payload = s };
CratonValue r = cratonvm_invoke_static(
    vm, "java/lang/Integer", "parseInt",
    "(Ljava/lang/String;)I", &arg, 1);
/* r.tag == 1 (INT), r.payload == 42 */

cratonvm_release_ref(vm, s);
cratonvm_destroy(vm);
```

A C header can be regenerated from the source with cbindgen by setting the
`CRATONVM_REGEN_HEADER` environment variable at build time (cbindgen is
intentionally not a Cargo dependency, so the default build graph is untouched).

## License

Licensed under the [Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0).

Copyright 2024-2026 Craton Software Company.
