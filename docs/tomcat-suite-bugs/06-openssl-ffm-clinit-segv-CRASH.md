# Bug 06 — OpenSSL Panama/FFM binding clinit NPE → JIT SEGV  (CRASH)

**Status:** OPEN. Real CratonVM crash (HotSpot PASSes).
**Severity:** High — a hard `EXCEPTION_ACCESS_VIOLATION` (process death), and the
trigger (`AprLifecycleListener` / `openssl_h` static init) is reachable from many
classes, not just the repro below.
**Repro class:** `org.apache.catalina.util.TestServerInfo` (rc=-1073741819).

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

## Next steps

- Symbolize the faulting RVA `0x9359DC` and the JIT frames with
  `CRATONVM_SYMBOLIZE` on a release-with-debug build to pin the exact crash site.
- Check the FFM `downcallHandle`/`SymbolLookup` path for absent symbols — make it
  return a throwing handle (or skip the openssl_h binding) instead of null.
- Re-test with `CRATONVM_DISABLE_JIT=1` to confirm whether the SEGV is the JIT
  exception-path handling vs the FFM layer itself.

## Reproduction

```
cratonvm.exe -Xmx2g -cp <cp> org.junit.runner.JUnitCore \
  org.apache.catalina.util.TestServerInfo        # CWD: apps/tomcat
# -> EXCEPTION_ACCESS_VIOLATION; HotSpot: PASS
```
