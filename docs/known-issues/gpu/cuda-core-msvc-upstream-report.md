TITLE
=====
cuda-core: remaining driver-flag sites in `simt/` still block the Windows/MSVC build with CUDA 13.3 (follow-up to #204)

BODY
====

## Summary

`cuda-core` does not compile for `x86_64-pc-windows-msvc` against a CUDA
13.3 toolkit. Same root cause as #204 ("adapt CUDA driver flag types"),
which is merged and intact — but #204 covered
`cuda-core/src/cudarc_shim.rs` and `cuda-async/src/device_future.rs`, and
the equivalent sites under `cuda-core/src/simt/` were not included.

17 errors, all the same signedness mismatch. Reproduced on `main`
@ `cdc69c13a752` and on the published `cuda-core` 0.3.1 — identical error
list and identical line numbers.

A patch is attached: 13 casts across 5 files, `cargo fmt` clean,
`cargo clippy` clean, and it applies to a pristine `main` checkout. Happy
to open it as a PR.

## Environment

| | |
| --- | --- |
| Target | `x86_64-pc-windows-msvc` |
| OS | Windows 11 (10.0.26200) |
| CUDA toolkit | 13.3 (V13.3.73) |
| rustc | 1.97.1 stable (`8bab26f4f`, 2026-07-14) |
| cuda-core | `main` @ `cdc69c13a752`, and 0.3.1 from crates.io |
| GPU | RTX 2060 (sm_75), driver 13030 |

## Reproduction

```
git clone https://github.com/NVlabs/cutile-rs && cd cutile-rs
cargo build -p cuda-core
```

(`bindgen` needs `libclang`; any 18.x `libclang.dll` on `LIBCLANG_PATH`
reproduces.)

## Errors

```
cuda-core\src\simt\context.rs:472:21: error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\context.rs:475:79: error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\context.rs:724:28: error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\context.rs:724:26: error[E0277]: no implementation for `u32 & i32`
cuda-core\src\simt\context.rs:724:82: error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\context.rs:724:80: error[E0277]: no implementation for `u32 | i32`
cuda-core\src\simt\context.rs:745:37: error[E0308]: expected `i32`, found `u32`
cuda-core\src\simt\context.rs:758:29: error[E0308]: expected `i32`, found `u32`
cuda-core\src\simt\context.rs:770:36: error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\event.rs:73:65:   error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\module.rs:652:17: error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\stream.rs:150:17: error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\stream.rs:198:17: error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\mod.rs:163:20:    error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\mod.rs:287:20:    error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\mod.rs:396:20:    error[E0308]: expected `u32`, found `i32`
cuda-core\src\simt\mod.rs:404:20:    error[E0308]: expected `u32`, found `i32`
```

## Root cause

The underlying type of a plain C enum is implementation-defined, and the
platforms disagree:

* **Linux / clang** — an enum whose enumerators are all non-negative gets
  an `unsigned int` underlying type.
* **Windows / MSVC** — a plain C enum is `int`, signed, regardless.

So on this target *every* generated driver enum is `c_int`:

```
$ grep -oE '^pub type [A-Za-z_]+_enum = [a-z0-9:_ ]+;' types.rs \
    | sed 's/.*= //' | sort | uniq -c
    113 ::std::os::raw::c_int;
```

113 of 113, uniformly. That is what rules out "CUDA version skew" and
identifies it as the platform ABI: the C prototypes really do declare
these parameters `unsigned int`, so the mismatch is between the generated
enum constant and the generated parameter type. Exactly the diagnosis in
#204; only the file list differs.

Two of the sites are the *reverse* direction: `DriverError(pub CUresult)`
is `c_int` here while `CudaContext::error_state` is an `AtomicU32`, so
`context.rs:758` and `:770` need casts the other way.

## Fix

`as _`, the convention #204 established — it keeps the source portable
instead of hard-coding either platform's choice:

```diff
-        let flags = cuda_bindings::CUstream_flags_enum_CU_STREAM_NON_BLOCKING;
+        let flags = cuda_bindings::CUstream_flags_enum_CU_STREAM_NON_BLOCKING as _;
```

13 casts: `context.rs` 5, `mod.rs` 4, `stream.rs` 2, `event.rs` 1,
`module.rs` 1. The two reverse-direction sites in `context.rs` and the
`CU_CTX_SCHED_MASK` read-modify-write need explicit `as u32` rather than
`as _`, because inference has nothing to fix on there.

With the patch applied to `main` @ `cdc69c13a752`:

```
cargo build  -p cuda-core   # 0 errors, 0 warnings
cargo fmt    -p cuda-core -- --check   # clean
cargo clippy -p cuda-core   # 0 errors
```

## Not a problem: `context.rs:293`

Worth stating because it looks like it should be one. `SyncPolicy::from_raw`
takes `raw: CUctx_flags_enum`, so `raw & CU_CTX_SCHED_MASK` is
`c_int & c_int` and is consistent on both platforms. It needs no cast, and
the build is clean without one.

## Two notes on the README, both measured on this box

Neither is a bug report; both are things a reader would currently infer
incorrectly, and both were checked against `main` @ `cdc69c13a752`.

**`cuda-core` builds on stable.** There is no `rust-toolchain.toml` at the
repo root and no `#![feature(...)]` anywhere under `cuda-core/src`. The
nightly requirement belongs to `cuda-host` and `rustc-codegen-cuda`. The
build above is stable 1.97.1.

**The host API works below the `sm_80` floor.** With the patch applied, a
runtime-PTX round trip — `load_module_from_ptx_src` → `load_function` →
`DeviceBuffer::from_host` → `launch_kernel_on_stream` → `to_host_vec` —
ran on an **sm_75** RTX 2060:

```
device = NVIDIA GeForce RTX 2060 (sm_75), driver = 13030
module loaded from runtime PTX; max_threads_per_block = 1024
mismatches = 0 of 1048576
```

The kernel is hand-written `.target sm_75` PTX, so no part of the cuTile
compiler is involved. That suggests the `sm_80` minimum is a cuTile
*kernel DSL* requirement rather than a constraint on the driver-side API,
and that "Linux (tested on Ubuntu 24.04)" is a portability gap rather than
a design limit. If that reading is right, both might be worth scoping more
narrowly in the README — a host-side user on Turing or on Windows would
currently conclude the crate is not for them.

## Context

I have been using `cuda-core` as an opt-in host backend in a JVM's
GPU-offload bridge. We lower JVM bytecode to PTX at run time, so
`load_module_from_ptx_src` is the entry point rather than `#[cuda_module]`.
It has been solid — this build break is the only thing keeping it
Linux-only for us.
