# The C ABI (`libcratonvm`)

`libcratonvm` builds a `cdylib`/`staticlib` (`libcratonvm.so` / `cratonvm.dll` /
`libcratonvm.a` / `cratonvm.lib`) exposing two C surfaces:

1. **The JNI Invocation API** — the three standard `libjvm` bootstrap entry
   points (`JNI_CreateJavaVM`, `JNI_GetDefaultJavaVMInitArgs`,
   `JNI_GetCreatedJavaVMs`). A host that already speaks JNI can load
   `libcratonvm` exactly as it would a real `libjvm.so` / `jvm.dll` and drive the
   `JNIEnv` function table by index.
2. **A flat `cratonvm_*` C API** — a curated convenience surface over **opaque
   handles + plain-old-data** (no Rust types cross the boundary). This is the
   easier path for most hosts: create a VM, load a class, make a string, invoke a
   method, read the result, destroy.

The authoritative signatures are in the generated header
`libcratonvm/include/cratonvm.h`, and ready-to-build C harnesses ship alongside
it (a flat-API example and a raw Invocation-API example).

## Building

```sh
cargo build -p libcratonvm   # produces target/debug/{libcratonvm.so, cratonvm.dll, ...}
```

Compile a host against it. On Linux:

```sh
cc embed.c -I libcratonvm/include -L target/debug -lcratonvm -ldl -lpthread -o embed
LD_LIBRARY_PATH=target/debug ./embed
```

On Windows (MSVC, linking the import library):

```bat
cl /Fe:embed.exe embed.c -I libcratonvm\include target\debug\cratonvm.dll.lib
copy target\debug\cratonvm.dll .
embed.exe
```

A static link against `libcratonvm.a` / `cratonvm.lib` works on any platform.

## Minimal flat-API example

```c
#include "cratonvm.h"
#include <stdio.h>

int main(void) {
    /* args may be NULL for defaults. The common HotSpot option forms are
     * understood: -Xmx<size>, -cp/-classpath <path>, -D<k>=<v>. */
    JavaVMOption opts[1] = { { (char *)"-Xmx64m", NULL } };
    JavaVMInitArgs args = { JNI_VERSION_1_8, 1, opts, /*ignoreUnrecognized=*/1 };

    CratonVm *vm = cratonvm_create(&args);          /* create + bootstrap */
    if (!vm) { fprintf(stderr, "%s\n", cratonvm_last_error(NULL)); return 1; }

    CratonClass cls = 0;
    if (cratonvm_load_class(vm, "java/lang/System", &cls) != JNI_OK)
        fprintf(stderr, "%s\n", cratonvm_last_error(vm));

    /* Invoke a static void method. The return is a tagged CratonValue;
     * tag == CRATON_TAG_ERROR signals failure, message in last_error. */
    CratonValue r = cratonvm_invoke_static(vm, "java/lang/System", "gc", "()V", NULL, 0);
    if (r.tag == CRATON_TAG_ERROR) fprintf(stderr, "%s\n", cratonvm_last_error(vm));

    /* Make a String, dispatch String.length() virtually, read it back. */
    CratonRef s = cratonvm_new_string(vm, "hello");
    CratonValue len = cratonvm_invoke_virtual(vm, s, "length", "()I", NULL, 0);
    printf("length = %d\n", (int)len.payload);         /* 5 */

    char *back = cratonvm_string_utf8(vm, s);          /* caller-owned buffer */
    printf("string = %s\n", back);
    cratonvm_free_string(back);                        /* must free it */

    cratonvm_destroy(vm);                              /* tear down */
    return 0;
}
```

## The flat API at a glance

| Function | Purpose |
|----------|---------|
| `cratonvm_create(const JavaVMInitArgs*)` | Build + bootstrap a VM; returns an owning `CratonVm*` (NULL on failure). |
| `cratonvm_destroy(CratonVm*)` | Drop the VM and free the handle. NULL is a no-op. |
| `cratonvm_load_class(vm, name, out_class)` | Load/link a class by internal name (`"java/lang/System"`). |
| `cratonvm_invoke_static(vm, cls, method, sig, args, n)` | Call a static method by descriptor (`"(I)I"`). |
| `cratonvm_invoke_virtual(vm, recv, method, sig, args, n)` | Virtual dispatch on a receiver (descriptor **excludes** the receiver). |
| `cratonvm_new_string(vm, utf8)` | Intern a `java.lang.String`; returns a `CratonRef`. |
| `cratonvm_string_utf8(vm, ref)` / `cratonvm_free_string(buf)` | Read a String back to UTF-8 (caller frees the buffer). |
| `cratonvm_object_class` / `cratonvm_class_name` | Runtime class of an object / its internal name. |
| `cratonvm_field_count` / `cratonvm_field_index` | Field layout-slot count / resolve a field name to a slot. |
| `cratonvm_get_field` / `cratonvm_set_field` (and `*_by_name`) | Read/write an instance field (writes are GC-barrier correct). |
| `cratonvm_last_error(vm)` / `cratonvm_clear_error(vm)` | This thread's pending error message. |

`CratonValue` is a `#[repr(C)]` tagged value (`tag` + 8-byte `payload`): `INT`,
`LONG`, `FLOAT`/`DOUBLE` (bit-cast), `OBJECT` (a `CratonRef`, `0` == null),
`VOID`, and `ERROR`. Method arguments are passed as a typed `CratonValue` array
plus a count — **not** C varargs (varargs across FFI are unsound for non-int
types and not ABI-portable).

## Handle, lifetime, and threading rules

- **Opaque handles.** `CratonVm*` is owning and opaque; the only valid
  operations are passing it to other `cratonvm_*` functions and finally to
  `cratonvm_destroy`. After destroy the pointer is dangling — don't reuse it.
  `CratonRef`/`CratonClass` are `u64` tokens; `0` is null for a `CratonRef` (but
  `0` is a *valid* `CratonClass` — `java/lang/Object` — which is why
  class-returning functions use an out-pointer plus a return code).
- **One handle, one thread.** Each flat call forms a unique borrow of the VM for
  its duration. Drive a given `CratonVm*` from a single thread.
- **Errors are per-thread.** `cratonvm_last_error` mirrors JNI's per-thread
  pending-exception model. Every entry point clears the error on entry, so check
  it immediately after a failing call. The returned pointer is library-owned and
  valid only until the next `cratonvm_*` call on this thread — copy it if you
  need it longer.
- **Panic safety.** Every `extern "C"` entry point wraps its body so a Rust
  panic becomes an error return (`JNI_ERR` / a `CRATON_TAG_ERROR` value / a null
  handle), never unwinding across the C boundary.
- **One VM per process (Invocation API).** A second `JNI_CreateJavaVM` returns
  `JNI_EEXIST`, mirroring HotSpot. The flat `cratonvm_create` path doesn't touch
  that process-global registry, but running many VMs in one process is still not
  a tested configuration.

> **After-destroy caution.** `cratonvm_destroy` drops the VM and releases its
> resources, but does not precisely scrub a stale JNI-context pointer out of the
> calling thread's thread-local storage. In the single-VM-per-thread usage this
> API is built for, this is benign — but do not call into a `JNIEnv*` after
> destroying the VM it belonged to.

## What's not on the flat API yet

- No static-field or fully reflective accessor — drop to the `JNIEnv` reflection
  table for those.
- A thrown Java exception surfaces as a handle (and class), not a message;
  reading `getMessage()` is a follow-up call you make yourself.

See [Embedding Overview](overview.md) for the lifecycle and the foreign-thread
caveat, and [The Rust Facade](rust-facade.md) if your host is Rust.
