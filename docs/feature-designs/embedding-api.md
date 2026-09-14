# `libcratonvm` embedding API

**Status:** Shipped (default on). All three layers are built and run against a
live JDK-25 VM.

## What it does today

`libcratonvm` is a workspace member built as `cdylib` + `staticlib` + `rlib`
(`libcratonvm/Cargo.toml`). `libcratonvm/src/lib.rs` is a real implementation:
`JNI_CreateJavaVM` constructs a `Vm`, runs the init phases, parks it in a
one-VM-per-process registry, publishes the thread-local JNI context and returns
the invocation and JNI tables.

- **JNI Invocation API.** `JNI_CreateJavaVM`,
  `JNI_GetDefaultJavaVMInitArgs` and `JNI_GetCreatedJavaVMs` are exported C
  symbols. `DestroyJavaVM`, `AttachCurrentThread`,
  `AttachCurrentThreadAsDaemon`, `DetachCurrentThread` and `GetEnv` are
  invocation-table slots implemented in `vm/src/native/jni.rs` — table slots,
  not exported symbols, which is what the ABI specifies.
- **Flat C ABI.** Roughly 28 `cratonvm_*` entry points: create / destroy,
  load-class, static and virtual invoke, string and object read-back, field
  read and write (by index, by name, and by descriptor via
  `cratonvm_field_index_desc`), thread enter/leave-native, ref release, and a
  `last_error` / `clear_error` channel. Header, header-only varargs shim and a
  C example live in `libcratonvm/include/` and `libcratonvm/examples/`.
  Header regeneration is opt-in via `CRATONVM_REGEN_HEADER`
  (`libcratonvm/build.rs`).
- **Layer-1 Rust facade.** `cratonvm-embed/src/lib.rs`, headless by default;
  `vm-defaults` / `vm-experimental` / `gpu-driver` are opt-in features.
- **Foreign threads work.** See
  [`foreign-thread-attach.md`](foreign-thread-attach.md).

## Known limits

- `libcratonvm` builds against a **non-default VM feature set** — `management`,
  `experimental-serialization`, `experimental-aot`, `experimental-debug`, and
  no `awt`. This is deliberate and recorded in its `Cargo.toml`, but it means
  the embedded VM is not byte-identical to the CLI's.
- **One VM per process.** Thread attach resolves through the process-global
  `PROCESS_VM` cell (`vm/src/native/jni.rs`), so attach ignores which
  `JavaVM*` the caller passes.
- **`GetEnv` ignores the requested version** and always returns the `JNIEnv*`,
  so a native JVMTI agent cannot obtain a `jvmtiEnv` through it. See
  [`jvmti-delivery-threading.md`](jvmti-delivery-threading.md).

## Goal

Ship a supported embedding surface so a non-CratonVM application (Rust *or* any
C-ABI host: C, C++, Go cgo, Python ctypes, a `libjvm`-replacement drop-in) can:
create a VM, attach threads, find classes/methods, invoke Java, exchange values,
and tear down — with a lifecycle and threading contract that matches the JNI
Invocation API closely enough to be a `libjvm.so`/`jvm.dll` substitute for
embedders that load the JVM via `JNI_CreateJavaVM`.

## Design

Three layers, each independently useful.

### Layer 1 — stabilize and version the Rust API (`cratonvm-embed`)

Promote the ad-hoc `Vm`/`SharedVm` surface to a curated, semver-stable facade:

- A thin `cratonvm-embed` crate (or a `pub` facade module in `vm`) that
  re-exports exactly the supported types (`Vm`, `VmConfig`, `Value`,
  `MethodCallFailed`, `JvmThread`, the native registration entry point) and
  hides the rest. The existing `vm/src/lib.rs` re-exports are the starting set.
- Document the **lifecycle** (`Vm::new` → init level 1 → `initPhaseN` →
  `invoke` → drop) and the **threading contract** already captured in
  `embedding.md` (one `JvmThread` per Java thread; `SharedVm` is `Send+Sync`;
  natives are immutable after construction). Pin these as API guarantees.
- Add the few obviously-missing conveniences embedders need: building a real
  `String[]` for `main`, reading a returned object's fields/string value,
  catching and inspecting a thrown `Throwable`.

### Layer 2 — the C-ABI `libcratonvm`

A `cdylib`+`staticlib` crate exposing a flat C API that wraps Layer 1:

- `crate-type = ["cdylib", "staticlib", "rlib"]` on a new `libcratonvm` crate.
- `extern "C"` entry points with opaque handles:
  `cratonvm_create(const Config*) -> Vm*`, `cratonvm_load_class`,
  `cratonvm_invoke_static`, `cratonvm_invoke_virtual`, `cratonvm_new_string`,
  `cratonvm_destroy`, plus value marshalling and a last-error/`Throwable`
  accessor. No Rust types cross the boundary; everything is handles + POD.
- A generated C header (`cbindgen`) and a minimal C example mirroring
  `embedding.md`'s Rust example.
- Panic safety: every `extern "C"` entry wraps the Rust call in
  `catch_unwind` and converts to an error code (a Rust panic must never unwind
  across the C boundary — UB).

### Layer 3 — JNI Invocation-API parity (`libjvm` substitute)

Implement the three exported bootstrap entry points so a host that links against
a JVM via the standard Invocation API can load `libcratonvm` as its `libjvm`:

- **`JNI_CreateJavaVM(JavaVM** pvm, void** penv, void* args)`**: parse
  `JavaVMInitArgs` (the `-Xmx`, `-cp`, `-D…` option strings) into a `VmConfig`,
  call `Vm::new`, publish the `JavaVM` function table (reuse the existing one in
  `jni.rs`) and the main-thread `JNIEnv`. This is the missing counterpart to the
  already-present `jni_destroy_java_vm`/`jni_get_env`/`AttachCurrentThread`.
- **`JNI_GetDefaultJavaVMInitArgs`** and **`JNI_GetCreatedJavaVMs`** (HotSpot
  allows at most one VM per process — mirror that; `embedding.md` already warns
  multiple `Vm` instances is untested).
- Wire the full Invocation function table (`DestroyJavaVM`, `AttachCurrentThread`,
  `AttachCurrentThreadAsDaemon`, `DetachCurrentThread`, `GetEnv`) — most slots
  already exist (`jni.rs:5137`–`5209`); `CreateJavaVM` + the global VM registry
  are the gap.
- With this, the existing per-`JNIEnv` function table (already used by native
  methods) serves embedder-driven calls too: a C host does
  `CreateJavaVM` → `FindClass`/`GetStaticMethodID`/`CallStaticVoidMethod`, all of
  which already exist on the `JNIEnv` table.

## Risks

- **Panic-across-FFI is UB**: every `extern "C"` boundary must `catch_unwind`.
  The existing `embedding.md` already flags that a panic leaves VM state
  "unspecified" — the C API must convert it to an error code, not propagate.
- **Process-global state** (`embedding.md` pitfalls: native-io sandbox roots,
  `OnceLock<&mut Vm>` hooks) makes multiple/restarted VMs fragile. Match
  HotSpot's one-VM-per-process and document the rest as unsupported.
- **Thread attachment + GC safepoints**: a host thread that calls in must be a
  registered `JvmThread` and must participate in safepoints, or the GC stalls /
  corrupts. The attach path must register the thread with the safepoint
  machinery, not just `thread_registry`.
- **Signal handlers**: CratonVM installs SIGSEGV/SIGBUS handlers
  (`embedding.md`); a `libjvm`-substitute host may have its own — the
  create path must chain, not replace.
- **ABI stability**: once a C header ships, the struct/enum layouts are a
  compatibility surface; keep them opaque-handle-based to avoid lock-in.
- **JNI spec breadth**: full Invocation + `JNIEnv` parity is large; scope Layer
  3 to the subset real embedders use (`CreateJavaVM`, find/call static & virtual,
  string/array marshalling, exception check) before chasing 100% of the table.

