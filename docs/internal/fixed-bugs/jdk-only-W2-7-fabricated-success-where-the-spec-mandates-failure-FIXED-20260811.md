> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> Vectors `RJdkJni`, `RJdkFailure` and `RJdkProcess` all pass in the 53/1 run. Four of the five inventory rows were FIXED in-lane; row #4, the only "OUT-OF-FILE PATCH", is applied — `native-builtins/src/phases_late.rs:2148-2222` now mints a real `java.lang.ProcessHandleImpl` with a per-VM memo instead of a fresh bare-interface object per call. The one row it listed as owned by another live lane (`ModuleLayer.findModule` fabricating a `Module` for any syntactically valid name) was closed by W2-3.
>
> Previous location: `docs/known-issues/jdk-only/W2-7-fabricated-success-where-the-spec-mandates-failure.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# W2-7 — fabricated success where the spec mandates a failure

Status: fixed (4 in-file, 1 out-of-file patch), inventory extended.
Measured on 2026-08-06 against a fresh build, `--real-jdk` and `--jdk-only`,
with HotSpot 25.0.3 as the oracle.

## The species

CratonVM's synthetic layer answers "here you go" for things that do not exist.
The negative half of an API — the part that asserts a failure fails *correctly*
— then passes through silently, and the error surfaces much later or never.

The shape is always the same, and always looks harmless at the call site:

```rust
let _ = ctx.load_native_library(&lib_name);   // best-effort; errors are swallowed
Ok(None)
```

It is worse than a missing feature. A caller written against the JDK contract
(Netty's `NativeLibraryLoader`, Tomcat's `AprLifecycleListener`,
`ClassUtils.isPresent`, a `ModuleFinder` probe) *catches* the specified failure
and takes a documented fallback. Swallowing the failure removes the fallback and
leaves the caller believing a backend is armed that is not.

## Inventory and disposition

| # | Symptom | Spec answer | Site | Disposition |
|---|---------|-------------|------|-------------|
| 1 | `System.loadLibrary(<absent>)` returns normally | `UnsatisfiedLinkError` | `native-builtins/src/lang_system.rs` (all four of `System.load`, `System.loadLibrary`, `Runtime.load0`, `Runtime.loadLibrary0`) | FIXED |
| 2 | `ModuleFinder.ofSystem().find("cratonvm.absent")` is present | `Optional.empty()` | `native-builtins/src/reflect_annotations.rs` | FIXED |
| 3 | `Cipher.getInstance("<bogus>")` returns a synthetic `Cipher` | `NoSuchAlgorithmException` | `native-builtins/src/jca/cipher.rs` | FIXED |
| 4 | `ProcessHandle.current()` mints a fresh non-equal object per call, with a bare-interface allocation whose `onExit`/`children`/`descendants` fabricate answers | singleton `ProcessHandleImpl`; `IllegalStateException`; real process-tree snapshot | `native-builtins/src/phases_late.rs` | OUT-OF-FILE PATCH (lane W2-7 does not own this file) |
| 5 | `ProcessHandleImpl.isAlive0` reported EVERY pid alive on Windows | `-1` for a pid that names no process | `native-io/src/process.rs::foreign_pid_is_alive` | FIXED (new — found while reading past #4) |

`ModuleLayer.findModule` fabricating a `Module` for any syntactically valid name
(`native-builtins/src/jboss_jdkspecific.rs`, trips `RJdkFailure.java:274`) is the
same species and is owned by another live lane; it is listed here only so the
inventory is complete.

## Why each fix is shaped the way it is

**#1 — the allowlist is load-bearing, not a hedge.** CratonVM never `dlopen`s
`libzip`/`libnet`/`libnio`: every `Java_java_util_zip_*` entry point they exist to
supply is a Rust native in this process. On HotSpot those `loadLibrary` calls
SUCCEED, and `regression-suite/src/RJdkJni.java:189-202` asserts that at least one
JDK-shipped library loads. So the fix is "attempt the load; on failure, throw
unless the bare name is one this VM already provides" — `is_vm_provided_jdk_library`
in `lang_system.rs`. A blanket throw would have traded one divergence for another.
`System.load` (a PATH, not a name) never takes the exemption: a file that is not
there is an error however it is spelled.

The failure is deliberately NOT memoised. `RJdkFailure.java:269` asserts the
second attempt throws too, and a library can legitimately appear on
`java.library.path` between two calls.

**#2 — two sources unioned, so the answer can only shrink safely.** A module
counts as present if the VM's own module registry knows it (`module_packages`,
authoritative for anything resolved in this VM including the module path) OR it
is in the JDK-25 image list. A stale image list can therefore under-report a
future JDK's module, never invent one.

**#3 — over-inclusive on accept, precise on reject.** The pre-existing check
looked only at the MODE (rejecting `CCM`), so the base ALGORITHM was never
questioned. The new `cipher_algorithm_known` spans SunJCE's whole catalogue plus
the BouncyCastle names the corpus reaches. A too-narrow list would throw
`NoSuchAlgorithmException` for VALID input, which is strictly worse than the
fabrication being fixed.

**#4 — the identity memo alone would not have been enough.** Memoising
`current()` fixes `current().equals(current())` and `hashCode()`, but
`RJdkProcess.java:106` is `ProcessHandle.of(pid).get().equals(current())`, and
`ProcessHandle.of` is NOT registered — it runs real bytecode and yields a real
`java.lang.ProcessHandleImpl`, whose `equals` opens with
`obj instanceof ProcessHandleImpl`. A bare allocation under the
`java/lang/ProcessHandle` INTERFACE fails that test no matter how stable its
identity is. So `current()` must build a real `ProcessHandleImpl(pid, 0)`, the
way `native-io::process::build_process_handle` already does for
`Process.toHandle()`. `startTime = 0` is the JDK's own `STARTTIME_ANY` wildcard:
`ProcessHandleImpl.equals` (verified by `javap -c`) treats a zero start time on
EITHER side as matching.

Returning the real class also un-fabricates three answers in the same object:
`onExit()` becomes the specified `IllegalStateException` for the current process,
and `children()`/`descendants()` stop returning an empty stream that was
indistinguishable from a true empty answer.

**#5 — `true` for every pid is the same defect one layer down.** Once `#4` routes
`ProcessHandle.of` through real bytecode, `isAlive0` is what decides whether a pid
resolves. `foreign_pid_is_alive` answered an unconditional `true` off Linux, so
`ProcessHandle.of(Long.MAX_VALUE)` was PRESENT. `OpenProcess` +
`GetExitCodeProcess` is what HotSpot's own `ProcessHandleImpl_md.c` does;
`ERROR_ACCESS_DENIED` is reported alive (the process exists, we merely lack rights
to it), because the opposite answer would be the same fabrication inverted.

## How to find the next one

Grep for the three spellings that discard a computed failure:

```
let _ = ctx.
.unwrap_or(true)
.ok()          // where the Err arm carried the only "no" the API can say
```

then ask one question per hit: **what does the JDK do when this fails?** If the
answer is a named `Throwable` or an empty `Optional`, and the code returns
normally, it is this species.

A second, cheaper filter: any native whose body cannot answer "no" at all.
`p60_pid_is_alive` in `phases_late.rs` still returns a hard-coded `true` off Unix
(same defect as #5, on the synthetic-JDK fallback path); it is reachable only
when `java/lang/ProcessHandleImpl` is absent, which is why it is recorded here
rather than patched.
