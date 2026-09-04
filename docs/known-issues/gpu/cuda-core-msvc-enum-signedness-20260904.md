# `cuda-core` 0.3.1 does not compile on Windows/MSVC (enum signedness)

## Status

Upstream defect in a third-party crate, found 2026-09-04 on this Windows
box while building the `cuda-oxide` backend (`cuda-bridge/src/backend_oxide.rs`).
**Not a CratonVM bug and not fixed in-tree.** The backend is therefore
Linux-only until upstream fixes it; `--features cuda-oxide` will not
build on Windows/MSVC.

Reproduced deterministically: 17 errors, every one a signedness
mismatch. Verified fixable with 13 mechanical edits, and verified
working on this machine's GPU afterwards (see "It works once patched").

## What happens

```
cargo build -p cratonvm-cuda-bridge --features cuda-oxide
```

fails inside `cuda-core-0.3.1` itself — not in our code — with 17
errors of two shapes:

```
src\simt\context.rs:472:21: error[E0308]: mismatched types: expected `u32`, found `i32`
src\simt\context.rs:724:26: error[E0277]: no implementation for `u32 & i32`
```

spread across `error.rs`, `context.rs` (5), `event.rs`, `module.rs`,
`stream.rs` (2) and `mod.rs` (4).

## Cause

`cuda-bindings` generates the CUDA driver bindings with `bindgen` from
the vendor's `cuda.h` at build time. The underlying type of a plain C
enum is **implementation-defined**, and the two platforms disagree:

- **Linux/clang**: an enum whose enumerators are all non-negative gets
  an `unsigned int` underlying type.
- **Windows/MSVC**: a plain C enum is `int` — signed — regardless.

So on this host every generated driver enum is `c_int`:

```
$ grep -oE '^pub type [A-Za-z_]+_enum = [a-z0-9:_ ]+;' types.rs \
    | sed 's/.*= //' | sort | uniq -c
    113 ::std::os::raw::c_int;
```

113 of 113, uniformly — which is what rules out "CUDA version skew" and
identifies it as the platform ABI. `cuda-core` was written against the
Linux shape and passes those constants where the FFI function signature
says `unsigned int` (the C prototypes really do say `unsigned int Flags`),
so the mismatch is between the enum constant and the parameter.

This is consistent with the project's own stated support matrix —
cutile-rs's README lists **Linux (tested on Ubuntu 24.04)** only.

## It works once patched

The failure is entirely mechanical. 13 edits — casts at the call sites,
no logic change:

| File | Sites |
|---|---:|
| `src/simt/context.rs` | 5 |
| `src/simt/mod.rs` | 4 |
| `src/simt/stream.rs` | 2 |
| `src/simt/event.rs` | 1 |
| `src/simt/module.rs` | 1 |

With those applied via a local `[patch.crates-io]`, on this box
(Windows 11, CUDA 13.3, driver 13030, **RTX 2060 / sm_75**):

- `cuda-core` builds on **stable** Rust. It carries no `#![feature(...)]`
  of its own — only `cuda-host`, the crate we do *not* use, needs
  nightly.
- A runtime-PTX round trip works: `load_module_from_ptx_src` →
  `load_function` → `DeviceBuffer::from_host` → `launch_kernel_on_stream`
  → `to_host_vec` gave **0 mismatches over 1048576 elements**.
- The full `cuda-bridge` device suite passes on the backend built from
  it: **10 device tests**, including `concurrent_dispatch_it`'s
  one-context-many-threads shape, 6/6 runs green.

Two claims worth recording because they are easy to get wrong:

1. **The Linux-only note is a portability bug, not a design limit.**
   Nothing in `cuda-core` is Linux-specific; it is 13 casts away from
   building here.
2. **The `sm_80` minimum in cutile-rs's README does not bind us.** That
   floor is for the cuTile *kernel DSL*. `cuda-core`'s host API is
   driver calls, and it drove an sm_75 card without complaint.

The patch is deliberately **not** vendored in-tree. The repo does vendor
a patched `rustls` (see `Cargo.toml`'s `[patch.crates-io]`), but that
one buys a shipped feature's correctness. This one would buy a
Windows build of an opt-in, Linux-supported backend — not worth carrying
a fork of an NVIDIA crate for. Revisit if upstream declines the fix.

## What to do

- **Linux**: nothing; `--features cuda-oxide` builds against the
  unpatched crates.io release.
- **Windows**: use the default `cuda` (cudarc) backend. If you need to
  build the oxide backend here, apply the 13 casts to a local checkout
  of `cuda-core` 0.3.1 and add a `[patch.crates-io]` entry for it.
- **Upstream**: the fix is small and self-contained; worth reporting to
  NVlabs/cutile-rs.

## Pointers

- `cuda-bridge/src/backend_oxide.rs` — the backend, and its module docs
- `docs/gpu/cuda-oxide-evaluation.md` — why this backend exists at all
- <https://github.com/nvlabs/cutile-rs> — `cuda-core`'s repository
