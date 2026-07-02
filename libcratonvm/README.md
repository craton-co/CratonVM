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
`-D<key>=<value>`.

### 2. Flat opaque-handle C API (`cratonvm_*`)

A curated convenience surface over opaque handles + POD, for hosts that prefer
not to drive the raw JNIEnv table by slot index:

- **Lifecycle:** `cratonvm_create`, `cratonvm_destroy`
- **Classes:** `cratonvm_load_class`, `cratonvm_class_name`, `cratonvm_object_class`
- **Invocation:** `cratonvm_invoke_static`, `cratonvm_invoke_virtual`
- **Strings:** `cratonvm_new_string`, `cratonvm_string_utf8`, `cratonvm_free_string`
- **Fields:** `cratonvm_field_count`, `cratonvm_field_index`, `cratonvm_get_field`,
  `cratonvm_get_field_by_name`, `cratonvm_set_field`, `cratonvm_set_field_by_name`
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
`null`.

Every exported `extern "C"` entry point wraps its body in `catch_unwind`, so a Rust
panic never unwinds across the C boundary.

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

cratonvm_destroy(vm);
```

A C header can be regenerated from the source with cbindgen by setting the
`CRATONVM_REGEN_HEADER` environment variable at build time (cbindgen is
intentionally not a Cargo dependency, so the default build graph is untouched).

## License

Licensed under the [Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0).

Copyright 2024-2026 Craton Software Company.
