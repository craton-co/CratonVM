# `classloading -> jit` dependency inversion — fixed 2026-07-27

## Problem

The class-loading crate directly depended on the concrete JIT implementation
for two reasons:

1. `CachedInvokeTarget::Jit` stored
   `Arc<cratonvm_jit::CompiledMethod>`.
2. class registration called `cratonvm_jit::count_param_slots`.

This made a loader and resolution layer compile against executable-memory,
code-generation, and backend implementation details. It also prevented an
alternate compiler from sharing the invoke cache without modifying
`classloading`.

## Design

`CachedInvokeTarget` and `InvokeCache` are now generic over their compiled
artifact:

```text
CachedInvokeTarget<JitMethod = ()>
InvokeCache<JitMethod = ()>
```

The VM specializes both with `Arc<CompiledMethod>`. The generic is monomorphized,
so the boundary adds no trait-object call, downcast, allocation, or indirect
load to the invocation hot path. Class-loading-only tests keep the default
backend-free `()` specialization.

The compact calling-convention descriptor counter moved to `jit-api`; the
concrete JIT re-exports it for source compatibility. `classloading/Cargo.toml`
no longer lists `cratonvm-jit`.

The resulting layer direction is:

```text
classloading -> jit-api <- jit
vm ---------------------> jit
```

## Verification

- `cargo check -p cratonvm-classloading -p cratonvm-vm`
- `cargo test -p cratonvm-jit-api`
- invoke-cache unit tests in `cratonvm-classloading`
- dependency-tree assertion that `cratonvm-classloading` has no direct
  `cratonvm-jit` edge
- default-JIT semantic and interface-dispatch probes
