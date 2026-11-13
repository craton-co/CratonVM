# Embedding Overview

CratonVM can be embedded in another process and driven through an API instead of
being launched as the standalone `cratonvm` CLI. There are two supported front
doors; which one you want depends on your host language:

| Host language | Use | Crate / artifact |
|---------------|-----|------------------|
| **C / C++ / Go (cgo) / Python (ctypes) / any FFI** | the C ABI | [`libcratonvm`](c-abi.md) (`cdylib` / `staticlib`) |
| **Rust** | the curated facade | [`cratonvm-embed`](rust-facade.md) (rlib) |

Both sit on the same machinery the CLI uses (VM construction plus the bootstrap
initialization sequence). They are additive: a C host can use the convenient
flat handle API and still reach the raw JNI `JNIEnv` table when it needs a slot
the flat API doesn't expose.

## Maturity & threading model

> The embedding surface is **single-thread-friendly today**: drive a VM from the
> thread that created it.

- **One VM per process** is the only tested configuration (matching HotSpot).
  Process-global state — signal handlers, the native-I/O sandbox roots, a few
  one-time init hooks — makes many-short-lived or restarted VMs fragile.
- **One handle, one thread.** Each call forms a unique borrow of the VM, so a
  given VM handle must be driven from a single OS thread.
- **Foreign-thread call-in is not ready.** The JNI `AttachCurrentThread` slots
  exist, but a foreign OS thread that calls in must be registered with the GC
  safepoint machinery (not just the thread registry) or the collector can stall
  or corrupt the heap. Wiring that attach path is a deliberately out-of-scope
  item — see [Known Limitations](../java-support/limitations.md).
- **Initialization phases are caller-driven.** Construction bootstraps the VM to
  a state usable for static-method invocation; a full `main()`-style run still
  drives the `System.initPhaseN` sequence the way the CLI launcher does.

## Lifecycle at a glance

```text
create + bootstrap        (Vm::new / cratonvm_create / JNI_CreateJavaVM)
  → advance init phases    (the embedder drives System.initPhaseN as the CLI does)
  → invoke static/instance methods
  → destroy                (drop the VM and free the handle)
```

## Which door?

- **In Rust**, depend on [`cratonvm-embed`](rust-facade.md). It's a thin,
  semver-stable facade that re-exports exactly the supported types plus a few
  conveniences, so your build doesn't pin the whole internal VM crate's surface.
- **In any other language**, load [`libcratonvm`](c-abi.md). It exposes both the
  standard JNI Invocation API (so a host that already speaks JNI can load it like
  a real `libjvm`) and a curated flat `cratonvm_*` C API over opaque handles.

The repository ships ready-to-build C harnesses next to the generated header, and
the CLI launcher itself is the reference embedder.
