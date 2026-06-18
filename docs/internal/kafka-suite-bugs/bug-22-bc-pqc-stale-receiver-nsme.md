# Bug 22 — GC-stale invokevirtual receiver → bogus `NoSuchMethodError`

> **STATUS: RESOLVED on `9d8bba97` (no longer reproduces).** Real on the pre-merge
> binary (`cv-full2`); after dev merge `b5c709c7`, the clean isolated `kv2.exe` build
> shows **0 stale / 0 NSME** for `Tls12SslFactoryTest` across multiple default-mode
> runs (it now times out on the same network/JKS gap as bug-21, no abnormal exit).
> Root cause + `CRATONVM_SHADOW_STACK` mitigation retained below.

**Severity:** Medium (abnormal exit). Class:
`org.apache.kafka.common.security.ssl.Tls12SslFactoryTest` — **ABEND** on CratonVM
(HotSpot: FAIL — clean test failures, no abnormal exit). Reported per the task rule
(behaviour differs: CratonVM aborts the JVM with an uncaught `NoSuchMethodError`).

## Symptom

A burst of **stale-receiver** warnings, then an impossible method resolution:

```
WARN interpreter: Stale pointer detected in invokevirtual receiver (ptr=0x2ec4d630, all-zero header) — falling back to CP class java/util/function/Predicate
WARN interpreter: Stale pointer detected in invokevirtual receiver (ptr=0x2ec4c170, all-zero header) — falling back to CP class java/util/logging/Logger
... (Runnable, Class, String, ...) ...
WARN vm_exec: NoSuchMethodError method="org/bouncycastle/pqc/jcajce/provider/kyber/KyberKeyFactorySpi.ifPresent(Ljava/util/function/Consumer;)V"
Exception in thread "main" java/lang/NoSuchMethodError: org/bouncycastle/pqc/jcajce/provider/kyber/KyberKeyFactorySpi.ifPresent(Ljava/util/function/Consumer;)V
[cratonvm-cli] (no Java stack frames were captured for this exception)
```

`ifPresent(Consumer)` is `java.util.Optional`'s method; resolving it against
`KyberKeyFactorySpi` is nonsensical. The preceding "all-zero header" warnings are
the tell: **objects whose header has been zeroed after a GC** are flowing into
`invokevirtual` as receivers. The interpreter's salvage path falls back to the
*constant-pool* declared class, and when the salvage picks the wrong class the
virtual dispatch resolves a method that does not exist on it → `NoSuchMethodError`.

## Root cause — CONFIRMED

**Conservative JIT-frame root scanning misses live references that exist only in
machine registers** (not spilled to the scannable stack). The young non-moving sweep
frees a still-live object; the dangling reference is then read back from an object
field/slot, reads as an all-zero header, and the interpreter's salvage falls back to
the constant-pool class — which mis-resolves a method that doesn't exist on it
(`Object.close(Object,Object,Object)`, `Predicate.test` on a freed lambda) →
`NoSuchMethodError`.

This is the **interpreter-caught** form of [bug-21](bug-21-ssl-jit-sigsegv.md) (which
hard-faults in JIT code). The "register-invisibility" hazard is documented in
`project_precise_jit_stack_maps` / `reference_osr_main_corruptor`.

### How it was pinned down
- `CRATONVM_DBG_STALE_RECV=1`: the stale receiver is `this.abortedExecutionPredicate`
  in JUnit's `ThrowableCollector.hasAbortedExecution` — a lambda `Predicate`. The
  owner (`this`) is live at `0x30791428`; the predicate field points to `0x2ebfd630`
  in the **young from-space** arena that was swept.
- `CRATONVM_DBG_RSET_AUDIT=1`: **no** old→young write-barrier misses — so it is NOT a
  remembered-set/card bug. The young referent is lost because the *root scan* never
  saw it (it lived only in a JIT register at GC time), not because a barrier was
  skipped.
- `CRATONVM_NO_SELECTIVE_PROMOTE=1`: still reproduces — NOT selective promotion.
- `--nojit`: **0 stale / 0 NSME** (the class then fails on an unrelated JKS-keystore
  gap, as HotSpot does) — proves the lost roots are JIT-frame registers.
- `CRATONVM_SHADOW_STACK=1`: **0 stale / 0 NSME** — precise JIT roots pin the
  register-only references; the defect is gone.

## Fix

Run with **`CRATONVM_SHADOW_STACK=1`** (shadow-stack precise JIT roots). Eliminates
the stale-receiver `NoSuchMethodError` here and the SIGSEGV in bug-21. See bug-21 for
the default-on evaluation caveat (perf + bt18 GC-counting).

## Reproduce

```
cd apps/kafka/tests
# bug present:
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 ./cvk.exe -cp ".;$(cat cp.txt)" KRun \
  org.apache.kafka.common.security.ssl.Tls12SslFactoryTest
# bug gone:
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_SHADOW_STACK=1 ./cvk.exe \
  -cp ".;$(cat cp.txt)" KRun org.apache.kafka.common.security.ssl.Tls12SslFactoryTest
```

## Affected classes (append more as found)
- common.security.ssl.Tls12SslFactoryTest (ABEND)
