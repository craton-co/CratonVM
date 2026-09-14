# `cuda-core` 0.3.1 does not compile on Windows/MSVC (enum signedness)

## Status

Upstream defect in a third-party crate, found 2026-09-04 on this Windows
box while building the `cuda-oxide` backend (`cuda-bridge/src/backend_oxide.rs`).
**Not a CratonVM bug and not fixed in-tree.** The backend is therefore
Linux-only until upstream fixes it; `--features cuda-oxide` will not
build on Windows/MSVC.

Reproduced deterministically on BOTH the published `cuda-core` 0.3.1 and
upstream `main` @ `cdc69c13a752` -- 17 errors, identical line numbers,
every one a signedness mismatch. Verified fixable with 13 casts, and
verified working on this machine's GPU afterwards (see "It works once
patched").

**A fix is prepared and ready to submit** -- see "Upstream" at the end.

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

This is consistent with the project's own stated support matrix --
cutile-rs's README lists **Linux (tested on Ubuntu 24.04)** only.

### Upstream already fixed the same bug elsewhere

cutile-rs PR **#204**, "fix: adapt CUDA driver flag types", merged
2026-08-05, carries exactly this diagnosis: *"`cuda-core` exposes the
flag aliases as `i32`, while bindgen generates the corresponding CUDA
driver parameters as `c_uint` (`u32`). This prevents the crate from
compiling with the current CUDA 13.3 SDK on the MSVC target."*

It is merged and intact -- its six `as _` casts are present in 0.3.1 and
on `main`. But it covered `cuda-core/src/cudarc_shim.rs` and
`cuda-async/src/device_future.rs` only; the equivalent sites under
`cuda-core/src/simt/` were not included, and those are the 17 errors
here.

That matters for how this gets reported: it is a **follow-up to a merged
fix**, not a new bug and not a regression. A report that did not say so
would likely be closed as already-fixed. It also settles the convention
-- `as _`, which keeps the source portable instead of hard-coding either
platform's choice.

### Verified on Linux 2026-09-05 -- WSL2, this same GPU

The Windows census alone could only show one side of the ABI claim. WSL2
(Ubuntu, kernel 6.18 microsoft-standard-WSL2) has `/dev/dxg` and
`/usr/lib/wsl/lib/libcuda.so`, so `nvidia-smi` reports the same RTX 2060,
and the CUDA 13.3 headers on the Windows side are readable at
`/mnt/c/...` -- no toolkit install, and **no sudo**: rustup and the
`libclang` wheel both live in `$HOME`.

Same crate, same headers, same bindgen, same libclang 18.1.1; only the
target differs:

| | `c_uint` | `c_int` |
| --- | ---: | ---: |
| Linux (`x86_64-unknown-linux-gnu`, stable 1.98.1) | **111** | 2 |
| Windows (`x86_64-pc-windows-msvc`) | 0 | **113** |

The two that stay signed on Linux are exactly the two with a NEGATIVE
enumerator (`CU_SHAREDMEM_CARVEOUT_DEFAULT = -1`,
`CU_GRAPH_CHILD_GRAPH_OWNERSHIP_INVALID = -1`), so the rule is visible
operating and predicting its own exceptions.

Both README claims in the upstream report are now measured rather than
argued:

* unpatched `cuda-core` builds clean on Linux, on **stable**;
* the runtime-PTX round trip ran on **sm_75** there, unpatched:
  `mismatches = 0 of 1048576`.

Two traps on the way, both mine. `types.rs` is pretty-printed on Windows
and ONE 2 MB line on Linux, so a `^`-anchored census grep returned zero
matches -- a formatting assumption reading as "no enums". And Git Bash
rewrote `/mnt/c/...` into `C:/Program Files/Git/mnt/c/...` before
`wsl.exe` saw it, which silently emptied several earlier probes;
`MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1` is required on every call.

### `context.rs:293` is NOT affected

Worth recording because it looks like it should be. `SyncPolicy::from_raw`
takes `raw: CUctx_flags_enum`, so `raw & CU_CTX_SCHED_MASK` is
`c_int & c_int` and is consistent on both platforms. An early draft of
the upstream report listed it, inferred from a grep; building `main`
proves the compiler never complains about it and the fix is clean
without touching it.

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
- **Upstream**: prepared, not yet submitted (this box has no GitHub API
  credentials). Two artefacts were produced on 2026-09-04:

  * an issue write-up citing #204, the 17 errors, the 113/113 `c_int`
    census, and the sm_75 / stable-toolchain findings;
  * `0001-cuda-core-msvc-simt-flags.patch`, a `git am`-able commit
    against `main` @ `cdc69c13a752`.

  The patch was validated with upstream's own gates on this box:
  `cargo build -p cuda-core` 0 errors 0 warnings, `cargo fmt --check`
  clean, `cargo clippy` clean (they enforce `clippy::all`, PR #263), and
  `git am` applies to a pristine checkout.

  **Apply it with a plain `git am`.** cutile-rs's CONTRIBUTING requires a
  DCO sign-off on every commit, and the patch already carries one:

      From: victor-craton <victor@craton.com.ar>
      Signed-off-by: victor-craton <victor@craton.com.ar>

  Author and sign-off match, which is what a DCO bot checks (it compares
  those two, not the committer, so re-applying under a different local
  git identity is fine). The sign-off is baked in at the repository
  owner's explicit direction -- it is the submitter's own certification,
  so do NOT re-generate this patch under someone else's identity without
  asking them first.

## Files here

Both are checked in beside this page so the fix is reproducible from the
repo rather than from one machine's temp directory:

- `0001-cuda-core-msvc-simt-flags.patch` — the 13-cast fix as a `git am`
  commit against cutile-rs `main` @ `cdc69c13a752`. Line numbers are
  identical in the published 0.3.1, so it also rebuilds the local
  toolchain copy at `C:/craton/toolchain/cuda-core-0.3.1-win`.
- `cuda-core-msvc-upstream-report.md` — the issue write-up, ready to file
  at <https://github.com/NVlabs/cutile-rs/issues/new>.

## Pointers

- `cuda-bridge/src/backend_oxide.rs` — the backend, and its module docs
- `docs/gpu/cuda-oxide-evaluation.md` — why this backend exists at all
- <https://github.com/nvlabs/cutile-rs> — `cuda-core`'s repository
