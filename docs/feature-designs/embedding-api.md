# `libcratonvm` Embedding API

Status: **LANDED** — all three layers shipped, built, and run against a live
JDK-25 VM. The `libcratonvm` `cdylib`/`staticlib` exposes the JNI Invocation API
(`JNI_CreateJavaVM` / `GetDefaultJavaVMInitArgs` / `GetCreatedJavaVMs` /
`DestroyJavaVM`), the flat `cratonvm_*` opaque-handle C API (create/destroy,
load-class, static + virtual invoke, string/object/field read-back + write-back,
last-error), and GC-safe foreign-thread attach; the curated `cratonvm-embed`
crate is the Layer-1 Rust facade. See the increment log at the end of this doc
for what each piece covers. (The historical sections below describe the *design*
that was implemented — kept for rationale; "Current state (cited)" reflects the
pre-implementation gaps, now closed.) A stable public surface for hosting a
CratonVM JVM inside a host process — both a curated Rust API and a C-ABI
`libcratonvm` with JNI Invocation-API parity.

The three previously-"genuinely next" items are now **DONE** (Increment 7
below): descriptor-based disambiguation of shadowed same-name fields
(`cratonvm_field_index_desc`); the C-varargs convenience shim (header-only
`cratonvm_helpers.h`); and CI publication of the `.so`/`.dll`/`.a` + header
(the `publish-libcratonvm` CI job). No open follow-ups remain on this feature.

## Goal

Ship a supported embedding surface so a non-CratonVM application (Rust *or* any
C-ABI host: C, C++, Go cgo, Python ctypes, a `libjvm`-replacement drop-in) can:
create a VM, attach threads, find classes/methods, invoke Java, exchange values,
and tear down — with a lifecycle and threading contract that matches the JNI
Invocation API closely enough to be a `libjvm.so`/`jvm.dll` substitute for
embedders that load the JVM via `JNI_CreateJavaVM`.

## Current state (cited)

- **A Rust-native embedding path already works and is documented.**
  `docs/EMBEDDING_VM_CRATE.md` is the current guide: `Vm::new(VmConfig)` does the
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
  "rlib"]`, registered in the workspace-root `Cargo.toml` `members`. cargo names
  the artifacts after the crate, so the real outputs are: **Windows**
  `libcratonvm.dll` + import lib `libcratonvm.dll.lib` + staticlib
  `libcratonvm.lib`; **Linux** `liblibcratonvm.so` + `liblibcratonvm.a` (the
  doubled `lib` is cargo's prefix on a crate already named `lib…`; linked with
  `-llibcratonvm`). The lib is *not* renamed to `cratonvm` because the
  `cratonvm-cli` bin already owns that name — a `cratonvm` cdylib would collide
  with `cratonvm.pdb` on `--workspace` Windows builds. (See the acceptance
  increment below; an earlier draft of this line claimed `cratonvm.dll` /
  `cratonvm.lib`, which cargo does not actually emit.)
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

## Increment 2 (Layer 2 flat C API) landed

The flat **opaque-handle C API** (Layer 2 in the design above) is implemented
alongside the Increment-1 Invocation-API entry points, in the same
`libcratonvm` crate (`libcratonvm/src/lib.rs`). No Rust types cross the
boundary — everything is handles + POD — and every entry point is
`#[no_mangle] pub extern "C"` and `catch_unwind`-wrapped (a panic converts to
an error code / null / `ERROR`-tagged value, never unwinds into C).

- **Opaque VM handle.** `cratonvm_create(const JavaVMInitArgs*) -> *mut
  CratonVm` builds a VM via the *same* `Vm::new` + `bootstrap` +
  `set_jni_context_arc` path as `JNI_CreateJavaVM` (no duplicated bootstrap),
  and hands back an owning `CratonVm*`. `cratonvm_destroy(vm)` drops it (null
  is a no-op). Unlike the Invocation API, the flat handle's lifetime is
  caller-controlled and it does **not** touch the `CREATED_VM`
  one-VM-per-process registry — a flat-only host may create/destroy freely.
- **Operations:** `cratonvm_load_class(vm, name, *out_class)` (out-pointer +
  `JNI_OK`/`JNI_ERR` return, because `0` is a valid `ClassId`);
  `cratonvm_invoke_static(vm, cls, method, sig, *args, n_args) -> CratonValue`;
  `cratonvm_new_string(vm, utf8) -> CratonRef` (reuses the Layer-1 interning
  constructor `vm::create_java_string`).
- **Handle / value encoding.** `CratonRef` (`u64`) is `ObjectRef::as_ptr()`
  with `0 == null` — identical to the JNIEnv side's `JObject = u64`, so handles
  are interchangeable between the two surfaces. `CratonClass` (`u64`) is a
  widened `ClassId`. `CratonValue` is a `#[repr(C)]` tag+`u64` POD covering
  int/long/float/double/object/void/error. **Args are a typed `CratonValue`
  array + count, not C varargs** — varargs across FFI are unsound for
  non-`int`/`double` types and not ABI-portable; a varargs convenience shim is
  noted as a next step.
- **Thread-local last error / pending-throwable accessor.**
  `cratonvm_last_error(vm) -> *const c_char` returns this thread's pending
  message (or null); `cratonvm_clear_error(vm)` clears it. State is
  thread-local, mirroring JNI's per-thread pending exception; each entry point
  clears it on entry. A thrown Java exception is reported with its throwable
  handle (reading the throwable's message/class needs heap-header access owned
  by another work item, so it is surfaced as an inspectable handle rather than
  decoded here).
- **Header.** A hand-written, cbindgen-byte-compatible stub ships at
  `libcratonvm/include/cratonvm.h` (the flat ABI is small/stable enough to
  maintain by hand for now; wiring real cbindgen generation as a build step is
  a next step).
- **C harness:** `libcratonvm/examples/embed_flat.c` exercises create →
  load_class → new_string → invoke_static → last_error (deliberate bad-class
  path) → destroy. Not compiled by cargo; build commands are in the file
  header.
- **Rust smoke tests** (in `libcratonvm/src/lib.rs` `tests`): `CratonValue`
  tag round-trip for every variant; null-handle → `JNI_ERR`/error-value/null
  with the last error set (for `load_class`, `invoke_static`, `new_string`);
  last-error set/clear round-trip; `cratonvm_destroy(null)` no-op. A full
  live-VM round trip is gated behind `--cfg flat_api_live_vm` so the default
  unit run stays fast and JDK-independent (the increment-1 convention of not
  booting a VM in the default unit run).

Next: a C varargs convenience overload of `invoke_static`; real cbindgen header
generation; `cratonvm_invoke_virtual` + value/array read-back helpers; and the
Layer-1 curated `cratonvm-embed` Rust facade.

## Increment 3 (Layer 2 string read-back) landed

Closes the first read-back gap flagged in increment 2 — a host can now turn an
`OBJECT` result (e.g. an interned `String` returned from `cratonvm_invoke_static`)
back into bytes it can read.

- **`char *cratonvm_string_utf8(CratonVm *vm, CratonRef str)`** — reads a
  `java.lang.String` handle into a freshly-allocated NUL-terminated UTF-8 buffer,
  reusing the VM's own `vm::read_java_string` primitive (the same one the JNIEnv
  `GetStringUTFChars` slot uses; `SharedVm.heap` is `pub` and `vm_object` is
  re-exported under `cratonvm_vm::vm::*`, so no vm-crate change was needed).
  Returns NULL + last-error on a bad handle or a non-`String`. Interior NULs are
  stripped so `CString::new` cannot fail.
- **`void cratonvm_free_string(char *s)`** — releases that caller-owned buffer
  (the buffer is NOT the thread-local last-error buffer; this mirrors JNI's
  `GetStringUTFChars`/`ReleaseStringUTFChars` ownership split).
- Header `cratonvm.h` + the C harness comment + Rust tests updated: a non-live
  null-handle test for `cratonvm_string_utf8` and a `free_string(NULL)` no-op,
  plus a `new_string → string_utf8 → assert "embed" → free_string` round trip in
  the `--cfg flat_api_live_vm` test.
- Still next: `cratonvm_invoke_virtual` (needs a clean Vm virtual-dispatch entry
  — deferred), object-field read-back, the varargs overload, real cbindgen, and
  the `cratonvm-embed` Rust facade.

## Increment 4 (virtual dispatch + object read-back + Layer-1 facade + cbindgen) landed

Closes most of the increment-3 "still next" list — the conveniences a host needs
once it can create a VM and call statics.

- **`cratonvm_invoke_virtual(vm, receiver, method, sig, args, n_args)`** — the
  instance-method companion to `cratonvm_invoke_static`. Resolves the method
  against the receiver's **runtime** class (most-derived override = virtual
  dispatch), the same pattern `Vm::run_pending_finalizers` uses; `sig` excludes
  the receiver, which is passed separately and prepended as arg 0.
- **Object inspection / field read-back** (Layer 2): `cratonvm_object_class`
  (runtime class handle), `cratonvm_class_name` (class internal name, caller-owned
  buffer), `cratonvm_field_count` (the valid `get_field` index range), and
  `cratonvm_get_field(vm, obj, index)` (typed `CratonValue` read of a bounds-
  checked instance-field slot via the public `heap.get_field`). Field resolution
  is **by layout index**, not by name — a name-based resolver needs a
  class-layout-by-name accessor the VM does not yet expose (noted follow-up); a
  host maps name→index via reflection or a getter through
  `cratonvm_invoke_virtual`.
- **`cratonvm-embed` (new crate)** — the curated, semver-stable **Layer 1** Rust
  facade: re-exports exactly the supported types (`Vm`, `VmConfig`, `Value`,
  `MethodCallFailed`, `JvmThread`, `ClassId`, …), documents the lifecycle /
  threading contract as API guarantees, and adds the conveniences embedders reach
  for — `make_string_array` (build a `String[]` for `main`), `read_string`,
  `object_class_name`, `describe_failure`. `#![forbid(unsafe_code)]`; registered
  in the workspace `members`.
- **Real cbindgen wiring** — `libcratonvm/cbindgen.toml` + a `build.rs` that
  regenerates `include/cratonvm.h` via the `cbindgen` CLI **only** when
  `CRATONVM_REGEN_HEADER=1`. It is a **no-op by default** and cbindgen is
  deliberately *not* a Cargo dependency, so the default build graph / `Cargo.lock`
  are untouched and offline builds are unaffected; the hand-maintained header is
  kept byte-compatible with the config.
- **The varargs overload is intentionally NOT shipped** — a C-varargs *definition*
  (`extern "C" fn(...)`) is unsound/unavailable on stable Rust; the typed
  `CratonValue` array + count is the stable, ABI-portable form and the recommended
  shape. (Documented, not faked.)
- **Header + tests.** `cratonvm.h` declares all new functions; the live
  round-trip example (`--cfg flat_api_live_vm`) now also exercises `length()` via
  `invoke_virtual`, `object_class`/`class_name` (asserts `java/lang/String`), and
  bounds-checked `get_field`. Null-handle unit tests for every new entry point
  pass without a VM bootstrap; `cratonvm-embed` adds a re-export compile-pin + a
  `no_run` doc example.
- **Still next:** name-based field resolution (needs a VM layout-by-name
  accessor), object-field *write-back*, and a C-varargs *convenience shim* layered
  over the typed-array core (if a host ABI ever needs it). *(Field resolution +
  write-back landed in Increment 5 below.)*

## Increment 5 (name-based field resolution + object-field write-back) landed

Closes the two field-access follow-ups Increment 4 deferred. The GC-correctness
and layout logic live in the **vm crate** (the right layer); the C ABI and Rust
facade are thin wrappers.

- **VM accessors (`vm/src/vm/vm_init.rs`, `impl Vm`).** `instance_field_index`
  (resolve a field *name* → absolute slot via the existing hierarchy walk
  `resolve_field_index_in_hierarchy`, now `pub(crate)`; most-derived declaration
  wins), `instance_field_count`, `get_instance_field`, and `set_instance_field` —
  the last is **GC-barrier correct**, replicating the interpreter's `putfield`
  exactly (SATB `write_barrier_pre` on the overwritten ref + the post
  write-barrier fired inside `set_field`), so a moving/concurrent collector stays
  sound on host-driven writes.
- **Flat C API (`libcratonvm`).** `cratonvm_field_index(vm, cls, name, *out)`
  (name→index, out-param + JNI_OK/ERR since 0 is a valid index),
  `cratonvm_get_field_by_name`, `cratonvm_set_field(vm, obj, index, value)`
  (bounds-checked write-back), and `cratonvm_set_field_by_name`. No coercion — the
  caller's `CratonValue` tag must match the field's declared type.
- **Layer-1 facade (`cratonvm-embed`).** `field_index`, `get_field_by_name`,
  `set_field_by_name` free-function conveniences over the `Vm` accessors.
- **Header + tests.** `cratonvm.h` declares all four; null-handle unit tests for
  each (23 libcratonvm tests total); the `--cfg flat_api_live_vm` round trip now
  resolves `String.hash` by name, asserts the **name-based read equals the
  index-based read**, **writes it back and reads the new value**, and confirms an
  unknown field name errors cleanly — **passing on a live JDK-25 VM**.
- **Still next:** descriptor-based disambiguation of shadowed same-name fields
  (resolution is by name today), and the C-varargs convenience shim (the typed
  `CratonValue` array remains the stable core; a definition-side C-varargs entry
  is unsound on stable Rust).

## Effort

L. Layer 1 (curate/document the existing Rust API) is S–M and immediately
shippable. Layer 2 (`libcratonvm` C-ABI) is M. Layer 3 (`JNI_CreateJavaVM` +
Invocation parity) is M–L but builds on the substantial JNI table already in
`jni.rs` — the creation entry point + global registry is the real gap, not the
function table.

## Acceptance landed — built + run against a live JDK-25 VM

Increments 1–5 were *coded* but the load-bearing proof — actually building the
`cdylib`/`staticlib` and running a C host against it — had never been executed
(the harness headers explicitly deferred the build to "the orchestrator"). That
is now done; the feature is verified end-to-end.

- **Library built.** `cargo build --release -p libcratonvm` produces
  `target/release/libcratonvm.dll` + `libcratonvm.dll.lib` (+ `.lib` staticlib).
  On a fresh worktree this hits the known libffi-sys MSVC `fficonfig.h` failure;
  the fix (prepend libffi's four header dirs to `INCLUDE` *before* vcvars) is
  encoded in the new reproducible wrapper `scripts/build-libcratonvm.ps1`.
- **`embed_smoke.c` (JNI Invocation API) — PASS.** A C host does
  `JNI_GetDefaultJavaVMInitArgs` → `JNI_CreateJavaVM(-Xmx64m, -D…)` →
  `JNI_GetCreatedJavaVMs` (reports 1) → `FindClass(java/lang/System)` →
  `GetStaticMethodID(gc,()V)` → `CallStaticVoidMethodA` through the live JNIEnv
  table. Exit 0, `embed_smoke: OK`. **This is the task's acceptance criterion: a
  C harness creates a VM and calls a static method.**
- **`embed_flat.c` (flat `cratonvm_*` API) — PASS.** create → load_class →
  new_string → invoke_static(System.gc) → deliberate bad-class last-error →
  destroy. Exit 0, with the error path returning a real message
  (`class not found: no/such/Class`).
- **Full flat surface (increments 3–5) — PASS** through the shipped `.dll`,
  `#include`-ing the public `cratonvm.h` (so the header is proven to compile from
  C and match the ABI): `new_string`+`string_utf8` round trip; `invoke_virtual`
  `"hello".length() == 5`; `object_class`+`class_name == "java/lang/String"`;
  name-based `field_index`/`get_field_by_name`/`set_field_by_name` write-back on
  `String.hash` (by-name read == by-index read; write 0x4d2 reads back 0x4d2);
  unknown field name errors cleanly.
- **Rust tests green:** `cargo test --release -p libcratonvm -p cratonvm-embed`
  = 23 + 1 + 1 doc-test, all pass.

**One real bug fixed.** `embed_smoke.c` modelled a JNIEnv table slot as bare
`void` (`typedef const void **JNIEnv; … (*env)[index]`). GCC/Clang allow
void-pointer arithmetic as an extension, so it "worked" on paper, but **MSVC
rejects it** (`C2036/C2069: 'const void *': unknown size`) — i.e. the Windows
acceptance harness never actually compiled. Fixed by modelling a slot as a sized
`void *` through a small typedef chain (`JniSlot`→`JniTable`→`JNIEnv`), matching
the VM's `JNIEnv = *const *const usize` double indirection. The slot indices
(`FindClass`=6, `GetStaticMethodID`=113, `CallStaticVoidMethodA`=143) were
verified against `build_function_table` in `jni.rs` and are correct.

**Doc/harness corrections.** The earlier "Produces `cratonvm.dll`/`cratonvm.lib`"
claim was wrong (cargo emits `libcratonvm.*`); both harness header build-command
blocks and the Increment-1 text were corrected to the real per-platform names,
and `embed_smoke.c`'s malformed Linux `cc` line was fixed.

- **Reproduce:** `pwsh -File scripts/build-libcratonvm.ps1` (auto-discovers
  vcvars + the libffi `INCLUDE` dirs, builds the lib, then compiles & runs both
  harnesses with unique exe names). `-NoRun` to build only, `-Profile dev` for a
  debug build, `-Suffix <tag>` to rename the harness exes.

Still genuinely next (unchanged): descriptor-based disambiguation of shadowed
same-name fields; a C-varargs convenience shim; and CI publication of the `.so`/
`.dll`/`.a` + header. (Foreign call-in thread registration with the GC safepoint
machinery — previously listed here as out of scope — is now **landed**; see
below.)

## Foreign call-in thread registration landed (GC-safe AttachCurrentThread)

`AttachCurrentThread` / `AttachCurrentThreadAsDaemon` now register a genuinely
foreign (host-created) OS thread as a first-class, GC-safe Java thread, closing
the gap that previously made "drive the VM only from the creating thread" the
sole safe usage. Design + rationale: `foreign-thread-attach.md`. Summary of what
shipped:

- **`JavaVM*` → live VM.** `JNI_CreateJavaVM` / `cratonvm_create` publish a
  process-global `Weak<SharedVm>` cell (`jni::set_process_vm` / `process_vm`)
  that the attach path upgrades — there is one VM per process, so "the JavaVM*"
  and "the one VM" denote the same fact.
- **Attach.** Builds a heap-boxed `JvmThread` with a fresh `ThreadId`, registers
  it in the `ThreadRegistry`, and mirrors its shared `Arc` fields (root_snapshot
  / gc_block_state / interrupted / park_state / frame_trace) so a GC initiator on
  another thread can scan and maintain its roots — the exact wiring a
  `Thread.start` worker gets. The box is parked in TLS (address-stable for the
  JIT's baked `tlab`/`shadow_stack` offsets). Registration happens *before* the
  JNI TLS context is published, so there is no window where the thread can run
  Java while invisible to `request_stw`. `JavaVMAttachArgs.name` is honoured.
- **Safepoint participation.** While running a Java call the thread is a counted
  mutator whose interpreter polls `stw_requested` and arrives at the barrier,
  exactly like any VM thread. Between calls it is modelled as GC-blocked (idle in
  the host event loop, no Java frames) so a stop-the-world on another thread is
  not stalled waiting for it; the first call leaves the blocked region and the
  return re-enters it (`ForeignCallGuard`, scoped to the outermost call).
- **Detach.** Refuses a detach with a Java call in flight (`JNI_ERR`, matching
  HotSpot); otherwise marks the thread dead while still blocked-excluded, waits
  out any in-flight STW, then reclaims the `JvmThread` (retiring the TLAB).
- **Default + opt-out.** Real registration is the **default**; the historical
  env-only stub is the opt-out safety net (`CRATONVM_FOREIGN_ATTACH=0`).
- **Fix.** The historical `AttachCurrentThread` stub wrote `JNI_TABLE_PTR.load()`
  (the table-array pointer) as the `JNIEnv*` — one indirection too shallow, so
  `(*env)[slot]` jumped to garbage. It now hands back `get_jni_env()`
  (`&JNI_TABLE_PTR`), matching `GetEnv`.

**Validation.** Per-increment Rust unit tests (process-VM resolve; factory
register/share/detach; idle thread excluded from STW; `ForeignCallGuard`
idle↔running transitions) plus an opt-in concurrent-GC soak
(`libcratonvm` `foreign_attach_concurrent_gc_soak`, `--cfg foreign_attach_soak`):
`JNI_CreateJavaVM` once, 6 host threads each `AttachCurrentThread` + loop an
allocating static call while periodically forcing `System.gc()` (multi-thread
STW while siblings are mid-call) + `DetachCurrentThread`. Green JIT-on and
`--nojit` (~370 STW collections per run); no UAF/crash, no STW hang,
`alive_count` returns to baseline.

**Idle host thread / creating thread.** A thread that parks *outside* the VM
(e.g. an idle coordinator/creating thread in a host `join()`/event loop) while
foreign threads drive GC must declare itself in-native, or a worker's STW waits
for it forever — it never reaches a Java safepoint. Foreign attached threads
handle this automatically (idle-blocked model). The creating/coordinator thread
has no automatic hook, so the embedding API exposes an explicit primitive:

```c
cratonvm_thread_enter_native();   // exclude this thread from GC STW while idle
... host-side join() / event-loop poll ...
cratonvm_thread_leave_native();   // rejoin the mutator population
```

This mirrors HotSpot's `_thread_in_native` transition. It is a no-op for a
foreign attached thread (already auto-managed). The concurrent-GC soak uses it to
bracket the creating thread's host-side wait. (A future refinement could
auto-block the creating thread on return from `JNI_CreateJavaVM` and auto-leave
on the next VM call, removing the explicit calls.)

## Increment 6 (`DestroyJavaVM` real teardown) landed

`DestroyJavaVM` — invocation-table slot 3, the lifecycle counterpart to
`JNI_CreateJavaVM` — was a literal no-op stub (`jni_destroy_java_vm` returned
`JNI_OK` without doing anything). Increment 1's claim that "the invocation table
(slots 3–7) … were already complete" was correct only for the *wiring*: the slot
existed and dispatched, but the slot 3 *implementation* did nothing. It now tears
the VM down for real.

- **Teardown hook (vm crate, `native::jni`).** The `DestroyJavaVM` slot lives in
  the vm crate, but the VM *instance* is owned by the embedding layer
  (`libcratonvm`'s process-global `CREATED_VM` registry), which the vm crate
  cannot reach. So a small process-global hook (`set_destroy_vm_hook` /
  `run_destroy_vm_hook`) is registered by the embedder at create time; the slot
  delegates to it. The hook is **one-shot** (taken on first call), so a second
  `DestroyJavaVM` is a no-op `JNI_OK` — mirroring HotSpot, where the VM cannot be
  destroyed twice. With **no** hook registered (a VM built directly from Rust via
  `Vm::new`, no Invocation-API bootstrap) the slot has nothing process-global to
  drop and reports `JNI_OK`.
- **The hook (`libcratonvm::destroy_created_vm`).** Takes the parked VM out of
  `CREATED_VM` and drops it — releasing its `Arc<SharedVm>`, heap, and threads —
  then releases this VM's flat-API handle table (and the global refs pinning its
  objects as GC roots, in case the host mixed surfaces) and clears the calling
  thread's JNI TLS context (so a later JNIEnv-table call on that thread does not
  resolve a freed VM). Registered in `JNI_CreateJavaVM` right after the JNI
  context is published. Wrapped in `catch_unwind` (a drop panic must not unwind
  across the C `DestroyJavaVM` boundary). After it runs, `JNI_GetCreatedJavaVMs`
  honestly reports 0.
- **Threading contract.** We honour the load-bearing part of HotSpot's
  `DestroyJavaVM` (tear the VM down, release the one-VM-per-process slot) but do
  **not** block waiting for other non-daemon threads to exit: the embedding
  contract is that the host calls `DestroyJavaVM` from the creating thread once it
  has quiesced its own Java activity, exactly as an embedder drives a single VM.
- **Restart caveat (unchanged).** HotSpot does not support recreating a VM after
  `DestroyJavaVM`, and neither do we — CratonVM installs process-global signal
  handlers / sandbox roots and leaks the JNI function tables as process-lifetime
  singletons (see Risks). Clearing the registry makes the count honest and frees
  the VM instance; a *subsequent* `JNI_CreateJavaVM` in the same process is
  untested and unsupported. The flat API's caller-controlled `cratonvm_destroy`
  (which already tore its handle down) is unchanged.
- **Tests.** vm crate: `destroy_vm_hook_runs_once_then_clears` (hook fires once,
  is one-shot, no-hook → `JNI_OK`). `libcratonvm`: `destroy_created_vm_without_vm_is_noop_ok`
  (no parked VM → idempotent `JNI_OK`). The `embed_smoke.c` Invocation-API
  acceptance harness now calls `DestroyJavaVM` (slot 3) after the `System.gc`
  call and asserts `JNI_GetCreatedJavaVMs` then reports **0** VMs.

## Increment 7 (the three remaining follow-ups) landed

Closes the last three "genuinely next" items, after which this feature has no
open follow-ups.

### Descriptor-based disambiguation of shadowed same-name fields

A subclass may re-declare a field with the same **name** as a super-class field
(field shadowing). Name-only resolution (`instance_field_index` /
`cratonvm_field_index`) always returns the most-derived declaration, so the
shadowed super-class slot was unreachable. Now resolvable by JVM type
descriptor:

- **vm crate.** `resolve_field_index_in_hierarchy_desc(class_id, name,
  descriptor: Option<&str>, store)` (the old `…_in_hierarchy` delegates with
  `None`) matches name **and**, when `descriptor` is `Some`, the field's declared
  descriptor (`ClassFileField.descriptor`). `instance_offset` still counts every
  non-static field, so the layout slot is unaffected by the filter. Exposed as
  `Vm::instance_field_index_desc`. (`None` is byte-identical to the name-only
  path; the most-derived match still wins, so two fields with the *same* name
  *and* descriptor remain indistinguishable by descriptor alone — full
  disambiguation there needs the declaring class, out of scope.)
- **Flat C API.** `cratonvm_field_index_desc(vm, cls, name, descriptor,
  out_index)` — `descriptor` may be **null** (≡ `cratonvm_field_index`). The
  resolved index feeds the existing `cratonvm_get_field` / `cratonvm_set_field`.
- **Facade.** `cratonvm_embed::field_index_desc`.
- **Tests.** `field_index_desc_null_handle_returns_err` (both with and without a
  descriptor); the `--cfg flat_api_live_vm` round trip asserts `String.hash`
  resolves identically by `"I"` and by null descriptor, and that `"J"` finds no
  match.

### C-varargs convenience shim — `cratonvm_helpers.h`

A definition-side C-varargs entry (`extern "C" fn(...)`) is unsound/unavailable
on stable Rust and not ABI-portable for non-`int`/`double` args, so the stable
core ABI keeps the typed `CratonValue` array + count. The varargs-like
*ergonomics* are restored **purely on the C side**, header-only, with no new ABI
surface and no Rust change:

- `cratonvm_val_int/long/float/double/object/void` build a `CratonValue` from a
  native C value (float/double bit-cast via `memcpy`); `cratonvm_as_*` /
  `cratonvm_is_error` read one back.
- `cratonvm_invoke_static_v` / `cratonvm_invoke_virtual_v` are variadic macros
  that assemble the typed array + count via a C99 compound literal + `sizeof`
  (so `cratonvm_invoke_virtual_v(vm, s, "substring", "(I)Ljava/lang/String;",
  cratonvm_val_int(2))` just works). They require ≥1 arg (a C99 compound literal
  cannot be empty); for a zero-arg call use the core function with `NULL, 0`.
- Kept in a **separate** header (not `cratonvm.h`) so the core header stays a
  faithful, cbindgen-regenerable mirror of the Rust ABI. `cratonvm.h` itself is
  unchanged except for the new `cratonvm_field_index_desc` declaration.
- Exercised by the new `libcratonvm/examples/embed_helpers.c`, which (unlike the
  re-declaring `embed_smoke.c`/`embed_flat.c`) actually `#include`s **both**
  public headers — so it doubles as proof they compile from C — and runs
  `Integer.toString(1234)` (static-`_v`), `"embed".substring(2)` / `.charAt(0)`
  (virtual-`_v`), and the `field_index_desc` descriptor cases.

### CI publication of the `.so`/`.dll`/`.a` + header

New `publish-libcratonvm` job in `.github/workflows/ci.yml` (matrix
ubuntu-latest + windows-latest): `cargo build --release -p libcratonvm`, stage
the per-platform artifacts (Linux `liblibcratonvm.so` + `liblibcratonvm.a`;
Windows `libcratonvm.dll` + `libcratonvm.dll.lib` + `libcratonvm.lib`) plus
`cratonvm.h` + `cratonvm_helpers.h`, and upload via `actions/upload-artifact@v4`
(`if-no-files-found: error`). The existing `build-and-test` job already compiles
the whole workspace incl. libcratonvm, so this adds no new build-failure surface
— only a release build + upload.

The reproducible local build+run wrapper `scripts/build-libcratonvm.ps1` now also
compiles & runs `embed_helpers.c` (a third harness, `-I libcratonvm/include` so
it can include the public headers).
