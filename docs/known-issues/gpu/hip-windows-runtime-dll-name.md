# The HIP backend is Linux-only, and the Windows gap is a DLL name

**Recorded 2026-09-21.** Upstream gap, no action available here.

## What

`--features hip` builds on Windows and cannot load a runtime there.

Two independent reasons, either of which would be sufficient:

1. **`cubecl-hip-sys` says so.** Its README states "Works only on
   Linux", and its one end-to-end test is `#[cfg(target_os = "linux")]`.
   The Windows path is not tested upstream.
2. **The DLL name moved.** The HIP SDK for Windows renamed its runtime
   library to `amdhip64_6.dll` at ROCm 6.1.2. `cubecl-hip-sys`'s
   `dynamic.rs` looks for `amdhip64.dll` on Windows, so
   `is_available()` returns `false` on a machine that has a perfectly
   good HIP SDK installed.

## What that means in practice

Nothing breaks. The second reason is the one that fires first, and it
fires into the guard: `is_available()` is `false`, so every entry point
in `backend_hip.rs` returns `DeviceError::NoDriver`, and the VM falls
back to the CPU exactly as it does on a machine with no GPU at all.
A Windows user of a `gpu-driver-hip` build gets correct answers and no
acceleration, which is the right failure.

The condition is covered by `cuda-bridge/tests/hip_no_driver_it.rs`,
which runs on every PR and asserts that this path reports rather than
panics.

## Why nothing is done about it here

Vendoring a DLL-name list would mean forking the loader, and the loader
is the reason this crate was chosen over the alternatives — it is what
makes `--features hip` build with no ROCm installed, which is what got
the backend into public CI at all. Carrying a patched copy to gain an
untested platform is a bad trade against carrying none.

## What would change it

Upstream adding `amdhip64_6.dll` to `library_names()`, which is a
two-line change and worth proposing. Until then the supported
configuration is Linux, and `docs/PLATFORMS.md` says so.

## Precedent

`docs/known-issues/gpu/cuda-core-msvc-enum-signedness-20260904.md`
records the same shape of thing for the oxide backend: an upstream
defect that confines a backend to one platform, written down so the
next person does not rediscover it by trying.
