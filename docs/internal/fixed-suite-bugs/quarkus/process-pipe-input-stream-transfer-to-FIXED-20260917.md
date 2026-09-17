# Quarkus: `NoSuchMethodError` on `ProcessPipeInputStream.transferTo` — FIXED

**Status: FIXED (2026-09-17).**

## Root cause

Exactly as diagnosed in the original report:
`cratonvm/synthetic/ProcessPipeInputStream` (`native-io/src/process.rs`)
registered `read()`/`read([BII)`/`read([B)`/`available()`/`close()`/
`readAllBytes()` directly, because its synthetic class chain never reaches
`java/io/InputStream` through normal VM virtual dispatch (same shape as the
`readAllBytes` gap already fixed and documented right above this one in
`process.rs`). `InputStream.transferTo(OutputStream)` — JDK 9+, ordinary
default-method bytecode — was never added to that direct-registration list,
so any caller of `p.getInputStream().transferTo(out)` (or
`getErrorStream().transferTo(...)`) on a CratonVM-spawned process hit
`NoSuchMethodError: 'long cratonvm.synthetic.ProcessPipeInputStream
.transferTo(java.io.OutputStream)'`.

## Fix

`native-io/src/process.rs`, in `register_process_natives`: registered
`transferTo(Ljava/io/OutputStream;)J` on `SYNTHETIC_PROCESS_INPUT_STREAM`,
delegating to the crate's existing generic `native_is_transfer_to` (the same
function `java/io/InputStream`'s own registration in `native-io/src/lib.rs`
uses) — one line, following the identical pattern the `readAllBytes`
registration two lines above it already established.

## Verification

Built `cratonvm` from a clean worktree and ran a standalone probe
(`ProcessBuilder` → `getInputStream().transferTo(ByteArrayOutputStream)`)
against it: before the fix, `NoSuchMethodError`; after, `transferred=17
rc=0`, output byte-identical to the same probe run under a real JDK. Full
`cratonvm-native-io` unit suite (534 tests) still green.
