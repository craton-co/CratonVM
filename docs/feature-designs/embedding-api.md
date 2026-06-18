# `libcratonvm` Embedding API

Status: design / not started (Rust-native embedding exists; C-ABI + JNI
Invocation API do not). L. A stable public surface for hosting a CratonVM JVM
inside a host process — both a curated Rust API and a C-ABI `libcratonvm` with
JNI Invocation-API parity.

## Goal

Ship a supported embedding surface so a non-CratonVM application (Rust *or* any
C-ABI host: C, C++, Go cgo, Python ctypes, a `libjvm`-replacement drop-in) can:
create a VM, attach threads, find classes/methods, invoke Java, exchange values,
and tear down — with a lifecycle and threading contract that matches the JNI
Invocation API closely enough to be a `libjvm.so`/`jvm.dll` substitute for
embedders that load the JVM via `JNI_CreateJavaVM`.

## Current state (cited)

- **A Rust-native embedding path already works and is documented.**
  `docs/internal/embedding.md` is the current guide: `Vm::new(VmConfig)` does the
  full bootstrap, `Vm::invoke(class, name, descriptor, &[Value])` drives calls,
  custom natives register via `SharedVm::native_methods`. The `Vm` struct is at
  `vm/src/vm/vm_init.rs:4253` with `impl Vm` at `:4261` exposing `new` (`:4263`),
  `load_class` (`:4411`), `new_object` (`:4427`), `new_array` (`:4441`),
  `new_ref_array` (`:4448`), `invoke` (`:4457`). `vm/src/lib.rs` re-exports the
  public symbols (`Vm`, `SharedVm`, `VmConfig`, `MethodCallFailed`, `JvmThread`).
- **`vm-cli` is the reference embedder.** Per `embedding.md`, the first-party
  `cratonvm` CLI wraps `cratonvm-vm`; `vm-cli/src/main.rs` is the explicit
  bootstrap loop (drives `System.initPhaseN`).
- **The JNI *function-table* side is substantially built; the *invocation* /
  *creation* side is partial.** `vm/src/native/jni.rs` has the per-`JNIEnv`
  function table (used by native methods) AND part of the Invocation Interface:
  - `JavaVM` type (`jni.rs:69`–`70`), `GetJavaVM` (`:2762`).
  - `jni_destroy_java_vm` (`:5140`), `jni_get_env` (`:5144`),
    `jni_attach_current_thread` (`:5156`), `jni_detach_current_thread` (`:5189`),
    `AttachCurrentThreadAsDaemon` (`:5209`).
  - **Missing: `JNI_CreateJavaVM`, `JNI_GetDefaultJavaVMInitArgs`,
    `JNI_GetCreatedJavaVMs`** — grep finds none. These are the three exported
    entry points a `libjvm` host calls to *bootstrap* a VM. Today the VM is
    created only from Rust (`Vm::new`), not via the C Invocation API.
- **No `cdylib`/`staticlib` is produced.** No `crate-type` is set in any
  `Cargo.toml` (grep finds none) — so there is no `libcratonvm.so`/`.dll`/`.a`
  artifact to link against from C. The only artifact is the `cratonvm` binary.

Net: a clean Rust embedding API exists and is documented; there is **no C-ABI
shared library** and **no `JNI_CreateJavaVM` bootstrap entry point**, so non-Rust
hosts and `libjvm`-replacement use cases are unserved.

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

## Implementation steps (ordered)

1. **Curate + document the Rust facade** (Layer 1): `cratonvm-embed`
   re-exporting the supported set; the lifecycle/threading guarantees from
   `embedding.md` become API contract. Add String[] / return-value / exception
   conveniences.
2. **`libcratonvm` cdylib/staticlib** (Layer 2): the flat C API + `catch_unwind`
   wrappers + cbindgen header + a C example.
3. **`JNI_CreateJavaVM` + the global VM registry** (Layer 3): config parsing,
   bootstrap, publish `JavaVM`/`JNIEnv`. Reuse the existing Invocation slots in
   `jni.rs`.
4. **`JNI_GetDefaultJavaVMInitArgs` / `JNI_GetCreatedJavaVMs`**; enforce
   one-VM-per-process.
5. **`libjvm`-substitute smoke test**: a C program (or a stock `java`-launcher
   style harness) that loads `libcratonvm` via the Invocation API and runs a
   `main`.
6. **CI artifacts**: build + publish the `.so`/`.dll`/`.a` and the header.

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

## Increment 1 (Layer 1 C-ABI) landed

The JNI **Invocation API** bootstrap layer — the `libjvm`-substitute entry
points that were the real gap (Layer 3 in the design above, shipped first
because it is the load-bearing piece a C host needs) — is implemented in a new
`libcratonvm` crate:

- **New crate `libcratonvm/`** with `crate-type = ["cdylib", "staticlib",
  "rlib"]`, registered in the workspace-root `Cargo.toml` `members`. Produces
  `libcratonvm.{so,dll,a}` / `cratonvm.dll` / `cratonvm.lib`.
- **Three exported entry points** (`#[no_mangle] pub extern "C"`):
  - `JNI_CreateJavaVM(JavaVM**, void** penv, void* args)` — parses
    `JavaVMInitArgs` (`-Xmx`, `-cp`/`-classpath`, `-D<k>=<v>`) into a
    `VmConfig`, calls `Vm::new`, runs the bootstrap init sequence
    (`System.initPhase1` best-effort + advance init level to 4, mirroring
    `vm-cli/src/main.rs`), sets the calling thread's JNI TLS context via the
    existing `set_jni_context_arc`, and hands back the **process-global**
    `JavaVM*` (`get_java_vm()`) and `JNIEnv*` (`get_jni_env()`) from
    `vm/src/native/jni.rs`.
  - `JNI_GetDefaultJavaVMInitArgs(void* args)` — writes the supported version
    (`JNI_VERSION_1_8`) into `JavaVMInitArgs.version`.
  - `JNI_GetCreatedJavaVMs(JavaVM**, jsize, jsize*)` — reports the at-most-one
    VM held in the process-global registry.
- **Reuse, not re-implementation:** the per-`Vm` `extern "C"` function tables
  in `jni.rs` are used verbatim. `jni.rs` itself was **not modified** — the
  invocation table (`DestroyJavaVM`/`AttachCurrentThread`/`DetachCurrentThread`/
  `GetEnv`/`AttachCurrentThreadAsDaemon`, slots 3–7) and the 234-slot `JNIEnv`
  table were already complete; the bootstrap/registry was the only gap, and it
  lives entirely in the new crate via the public `get_java_vm` / `get_jni_env`
  / `set_jni_context_arc` re-exports.
- **One-VM-per-process** is enforced (HotSpot semantics): a second
  `JNI_CreateJavaVM` returns `JNI_EEXIST`.
- **Panic safety:** every entry point wraps its body in `catch_unwind` and
  converts a caught panic to `JNI_ERR` (never unwinds across the C boundary).
- **Acceptance harness:** `libcratonvm/examples/embed_smoke.c` is a C program
  that does `GetDefaultInitArgs` → `CreateJavaVM` → `GetCreatedJavaVMs` →
  `FindClass`/`GetStaticMethodID`/`CallStaticVoidMethodA(System.gc)` through the
  `JNIEnv` table. It is not compiled by cargo; the orchestrator builds it
  against the produced library (build commands are in the file header).
- **Rust smoke tests** in `libcratonvm/src/lib.rs`:
  `JNI_GetDefaultJavaVMInitArgs` populates `version`; null-arg returns
  `JNI_ERR`; `JNI_GetCreatedJavaVMs` reports a valid count; `-Xmx`/classpath
  option parsing.

Next: Layer 2 (the flat `cratonvm_*` opaque-handle C API + cbindgen header) and
Layer 1 (the curated semver-stable `cratonvm-embed` Rust facade) build on this.

## Effort

L. Layer 1 (curate/document the existing Rust API) is S–M and immediately
shippable. Layer 2 (`libcratonvm` C-ABI) is M. Layer 3 (`JNI_CreateJavaVM` +
Invocation parity) is M–L but builds on the substantial JNI table already in
`jni.rs` — the creation entry point + global registry is the real gap, not the
function table.
