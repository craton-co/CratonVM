# Bug 06 — OpenSSL Panama/FFM binding clinit NPE → JIT SEGV  (CRASH)

**Status:** ✅ FIXED (commit `9ce4c3b4`, in `dev`). Re-verified 2026-06-15 — see
"Verification (2026-06-15)" below. The CRASH (SEGV) status in older suite runs is
stale (predates the fix); `TestServerInfo` no longer crashes under load.
**Severity:** High — a hard `EXCEPTION_ACCESS_VIOLATION` (process death), and the
trigger (`AprLifecycleListener` / `openssl_h` static init) is reachable from many
classes, not just the repro below.
**Repro class:** `org.apache.catalina.util.TestServerInfo` (rc=-1073741819).

## Verification (2026-06-15) — fix confirmed, residual failures are a test artifact

Re-checked on `dev` (fix `9ce4c3b4` present in `native-builtins/src/lang_reflect.rs`,
see "Root cause (symbolized) + FIX"). Worktree `CratonVM-tcbug0609`, branch
`fix/tomcat-bugs-0609-verify`.

- **The SEGV is gone.** `TestServerInfo` run **24× under concurrent CPU contention**
  (8× concurrent × 3 rounds) produced **0 hard crashes** (no
  `EXCEPTION_ACCESS_VIOLATION` / `0xC0000005` / `SIGSEGV`). Before the fix the doc's
  6× repro was a reliable 6/6 SEGV. Isolated run = `OK (22 tests)`.
- **The residual concurrent "FAILURES" are a test-harness artifact, NOT a VM bug.**
  Running `TestServerInfo` 6× concurrently now yields `rc=1` JUnit assertion
  failures (e.g. `Should read Bundle-Version expected:<1.2.3> but was:<null>`), all
  routed through `withTestJar` → `createTestJar`. `createTestJar`
  (`TestServerInfo.java:512`) writes the test JAR to `System.getProperty("java.io.tmpdir")`
  with a **fixed filename** and `withTestJar`'s `finally` deletes it — so N
  concurrent instances of *the same class* clobber/delete each other's shared temp
  JAR. **HotSpot reproduces the same failures** under 6× concurrent (seen 1/6), so
  it is a property of the upstream test, not CratonVM. CratonVM fails more often
  only because it runs the class slower (~15 s vs HotSpot's few s), widening the
  collision window.
- **It does not affect the real suite.** `run-suite.ps1` uses `parallel=5` but each
  *class* runs exactly once, so `TestServerInfo` runs as a **single instance**
  alongside 4 *different* classes — it gets the GC/CPU pressure that surfaced the
  SEGV (now fixed) but never the same-class temp-JAR collision. → expected suite
  result: **PASS**.
- We deliberately do **not** patch `createTestJar` to use a unique temp file: that
  would be modifying an upstream Tomcat test to paper over a HotSpot-shared race,
  which is out of scope (the VM bug — the SEGV — is the thing that was real, and
  it is fixed).

> **Update (2026-06-30):** the `openssl_h <clinit>` "Cannot invoke printf on null"
> NPE referenced throughout this doc as a *benign, caught* side note has itself
> been fixed — see [Bug 07](07-openssl-ffm-clinit-printf-npe-FIXED.md). `openssl_h`
> now fails with the *same* `IllegalArgumentException: Cannot open library:
> ssl.dll` as HotSpot (the behavior this doc called correct), instead of the
> CratonVM-specific printf NPE. The SEGV root cause below (GC stale-ref in
> `Method.invoke`) is unrelated and remains as documented.

## Symptom

```
ERROR [AprLifecycleListener] An incompatible version [2.0.38-cratonvm-stub] of
  the Apache Tomcat Native library is installed ...
... ExceptionInInitializerError class=org/apache/tomcat/util/openssl/openssl_h
    cause= java.lang.NullPointerException: Cannot invoke printf on null
#
# EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x...9359DC
#  Faulting access: read at address 0x000000001A9DF150
#  thread: "main-vm"
Native frames (most recent call first) [raw]:
   1: (external/jit)
   2: (external/jit)
   3: (external/jit)
   4: (exe+0x9359DC)
```

## Root cause (hypothesis)

The Tomcat OpenSSL **Panama/FFM** binding `org.apache.tomcat.util.openssl.
openssl_h` runs its `<clinit>`, which builds native downcall method handles. One
handle (`printf`) resolves to **null** (no real native OpenSSL/libc symbol bound
in CratonVM's FFM layer), so the clinit throws `NullPointerException: Cannot
invoke printf on null` → `ExceptionInInitializerError`. Downstream, a
**JIT-compiled** frame (the backtrace top is `external/jit`) then dereferences a
bad pointer and SEGVs (read at `0x1A9DF150`).

Two distinct defects likely compound:
1. **FFM downcall-handle resolution returns null instead of failing cleanly** —
   `Linker.downcallHandle(...)` / `SymbolLookup` for an absent symbol should
   yield a handle that throws a catchable exception, not a null that NPEs in
   clinit. (Or the absent-native path should be detected and the binding skipped,
   as HotSpot's APR listener does — it logs the incompatible-version warning and
   continues.)
2. **The follow-on SEGV in a JIT frame** — an `ExceptionInInitializerError`
   during a JIT-active path is mis-handled and corrupts control flow into a bad
   dereference rather than propagating the Java exception.

On HotSpot `TestServerInfo` PASSes: the APR/OpenSSL listener degrades gracefully
when the native library is absent.

## Refined finding (this session) — DEFERRED (deep FFM)

- The SEGV **reproduces with `CRATONVM_DISABLE_JIT=1`** — it is NOT a JIT bug. The
  "external/jit" frames in the backtrace are mislabeled FFM/libffi trampolines.
- Root cause confirmed in `native-builtins/src/panama.rs`: `openssl_h`'s FFM
  binding resolves OpenSSL symbols via `SymbolLookup` (`find` →
  `find_native_symbol`). Some symbols return Optional.empty() (e.g. `printf` →
  the null-handle NPE), but at least one resolves to a **valid-looking but
  wrong-ABI address** (a same-named system symbol), and `Linker.downcallHandle` +
  the libffi dispatch then **call that bad pointer → native SEGV**.
  `validated_fn_ptr` only rejects null/misaligned, not "wrong" addresses.
- A clean fix would require the OpenSSL FFM lookup to consistently FAIL when real
  OpenSSL isn't present (so `openssl_h` clinit throws cleanly and Tomcat disables
  OpenSSL, as HotSpot does on a no-OpenSSL box). But CratonVM's symbol resolution
  finds partial/wrong symbols, and tightening it risks breaking legitimate FFM
  (the Gradle worker path depends on FFM). So this is **deferred** as a dedicated
  foreign-function effort, NOT a quick edit.

## Re-diagnosis (branch `fix/tomcat-ffm-module-bugs`) — NOT an openssl-FFM logic bug

The "Refined finding" above is **wrong about the mechanism**. Verified this session:

- **The synthetic `panama.rs` FFM path is NOT active here.** `register_pe_panama`
  lives in `register_synthetic_overrides`, which is `#[cfg(feature="synthetic-jdk")]`
  — **off** in the default (real-JDK) build the suite runs. So the doc's
  "`panama.rs` resolves a wrong-ABI symbol" cannot be the cause. The real JDK FFM
  bytecode runs instead.
- **`openssl_h` init is CLEAN and a red herring.** Calling
  `org.apache.tomcat.util.net.openssl.panama.OpenSSLLibrary.init()` directly, or
  `ServerInfo.main(new String[0])` directly, completes with **no SEGV**:
  `openssl_h.<clinit>` fails with a *caught* `ExceptionInInitializerError`
  (NPE "Cannot invoke printf on null" — CratonVM's real-JDK `SymbolLookup`
  library-load path), exactly mirroring HotSpot's caught
  `IllegalArgumentException: Cannot open library: ssl.dll`. `isAvailable()` returns
  false either way. The logged `openssl_h <clinit> failed` line in the crash is
  just this benign, caught failure.
- **The SEGV is an intermittent, load/concurrency-dependent race**, not
  deterministic and not openssl-specific:
  - `JUnitCore TestServerInfo` run **sequentially in isolation passes** — `OK (22
    tests)` (seen 5/5, then later 0/3 — the rate is environment-dependent).
  - Run **6× concurrently** (CPU contention) it SEGVs **6/6**, crashing very early
    (only `JUnit version 4.13.2` printed — *before* any test method, before
    `openssl_h`). So the crash is during early class-loading/execution under load,
    unrelated to the openssl path that runs later.
  - It reproduces on the **unmodified `dev` binary** too — it is pre-existing, not
    introduced by the module-finder work on this branch.
- **Crash signature points to a stale reference after GC.** Always the same code
  site `pc = exe+0x935F7C`, faulting on a **read at `0x…F150`** — the high bits
  vary per run (`1BA0F150`, `1C9FF150`, `1CB5F150`) but the low 16 bits are
  constant. That is a field read at a fixed offset (~`0xF150`) from a base pointer
  that has moved/been freed — the classic stale-ref / moved-object pattern seen in
  the project's other GC SIGSEGV fixes (cf. `BUG-Z FileStore`, join-wakeup
  stale-ref, TLAB-tail desync). The backtrace's `external/jit` frames are
  non-exe addresses (loaded DLL / code region), not necessarily libffi.

So this is a **GC/concurrency stale-reference race surfaced under load**, in the
same family as the other GC SIGSEGV fixes — *not* a foreign-function symbol bug.
HotSpot is unaffected because its GC/threading is sound here.

## Root cause (symbolized) + FIX

Symbolized the crash on a release-with-debug build (6× concurrent repro). The
faulting frame is **`vm_exec::get_field_by_name` (vm_exec.rs:1845)** —
`class_id_of(obj)` on a stale object pointer — reached via:

```
get_field_by_name (vm_exec.rs:1845)            <- SIGSEGV
  method_descriptor_for_invoke (lang_class.rs:4146)
  native_method_invoke_boxed   (lang_reflect.rs:1068)   <- Method.invoke wrapper
  safe_native_call -> interpreter -> Vm::invoke
```

`native_method_invoke_boxed` (the `java.lang.reflect.Method.invoke` wrapper) did:

```
let raw = native_method_invoke(ctx, args)?;     // runs the TARGET method (can GC!)
...
let method_obj = args.first() ...;              // STALE: args is a pre-call snapshot
let descriptor = method_descriptor_for_invoke(ctx, method_obj);  // deref -> SIGSEGV
```

`native_method_invoke` executes arbitrary Java (the reflected target), which can
trigger a GC that **moves the `Method` mirror**. `args` is a snapshot of
operand-stack `Value`s held on the native's Rust stack — it is **not a GC root**,
so it is never remapped. Afterwards `args.first()` is a dangling pointer and
`method_descriptor_for_invoke` → `get_field_by_name` → `class_id_of` dereferences
freed/moved memory. JUnit drives every test method through `Method.invoke`, so the
window is hit constantly; under **CPU contention** (concurrent VMs / the suite
harness) a GC is far more likely to land inside the inner invoke, which is why it
presented as "intermittent, only under load". HotSpot keeps reflection args live
across the call, so it never faults.

**Fix** (`native-builtins/src/lang_reflect.rs`): capture the method descriptor
*before* the inner invoke, while the `Method` mirror is still valid (the
descriptor is invariant), and reuse it afterwards instead of re-reading the stale
`args` pointer. No more post-GC stale dereference.

This is **not** an FFM/openssl bug — the original "openssl-FFM symbol resolution"
diagnosis was wrong. `openssl_h` init fails cleanly and is caught; it merely ran
in the same early window where the GC race happened to fire.

## Verification

```
JUnitCore TestServerInfo, 6× concurrent (reliably 6/6 SIGSEGV before the fix):
  after fix -> 0 SIGSEGV, all OK (22 tests)            [see verification run]
```

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  org.apache.catalina.util.TestServerInfo        # CWD: apps/tomcat
# -> EXCEPTION_ACCESS_VIOLATION; HotSpot: PASS
```
