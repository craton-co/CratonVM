# `CassandraAutoConfigurationTests` HANG: JNI `ThrowNew` discards the real exception class/message, breaking `jnr-posix` provider-fallback during class init

**Status: OPEN — found 2026-07-17**

## Symptom

`module/spring-boot-cassandra` `CassandraAutoConfigurationTests` HANGs — 0
tests ever start. `.out.log` is empty (0 bytes). The last lines of
`.err.log` before the process goes silent (timed out by the harness at the
300s limit):

```
WARN vm_exec: Missing native method in real-JDK mode method=jdk/jfr/internal/JVM.subscribeLogLevel(...)
WARN vm_util: <clinit> failed — wrapping in ExceptionInInitializerError class=jnr/posix/POSIXFactory$DefaultLibCProvider$SingletonHolder cause=java/lang/IllegalStateException JNI ThrowNew pending exception
WARN vm_util:   [CLINIT-TRACE 0] at SbRunner.main (SbRunner.java:36) bci=133
   ... (19 more class-init-trace frames, all JUnit Platform launcher bootstrap, ending at)
WARN vm_util:   [CLINIT-TRACE 19] at org/junit/platform/engine/support/hierarchical/SameThreadHierarchicalTestExecutorService.submit (...) bci=1
```

Nothing follows. This happens synchronously on the single test-execution
thread, mid-bootstrap, before any test body runs.

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-cassandra.org.springframework.boot.cassandra.autoconfigure.CassandraAutoCon-c184e91472d0.out.log` (empty)
`...CassandraAutoCon-c184e91472d0.err.log`

## Root cause (CONFIRMED for the exception-substitution mechanism; HANG mechanism itself is a hypothesis)

CratonVM's JNI `ThrowNew` implementation discards the caller-supplied
exception class and message entirely:

- `vm/src/native/jni.rs:1880-1888`:
  ```rust
  extern "C" fn jni_throw_new(_env: JNIEnv, _clazz: JClass, _msg: *const c_char) -> JInt {
      // Store a marker that an exception was requested via ThrowNew.
      // The interpreter will check this on return from native code.
      JNI_PENDING_EXCEPTION.with(|cell| {
          cell.set(u64::MAX); // sentinel for "exception requested"
      });
      JNI_OK
  }
  ```
  Both `_clazz` and `_msg` are ignored (note the underscore-prefixed unused
  parameter names) — only a generic "exception requested" sentinel
  (`u64::MAX`) is recorded.
- `vm/src/vm/vm_exec.rs:1027-1039`: when that sentinel is observed on return
  from native code, CratonVM fabricates a **hardcoded, generic**
  `RuntimeError::IllegalStateException { message: "JNI ThrowNew pending
  exception" }` — regardless of what real exception class/message the
  native code actually intended to throw.

Every real native library that calls the standard JNI `ThrowNew` idiom (the
normal way native code signals an error — used pervasively by
`jnr-ffi`/`jnr-posix`'s native shim) has its real exception silently
replaced by this same generic `IllegalStateException`. Here,
`jnr/posix/POSIXFactory$DefaultLibCProvider$SingletonHolder`'s static
initializer — almost certainly probing for a native libc/POSIX binding on
Windows and expecting a specific, catchable exception type as part of its
normal provider-fallback logic (a standard jnr-ffi pattern: try a native
binding, catch a specific failure type, fall back to a pure-Java
implementation) — receives the wrong exception type, which it evidently
doesn't catch, so it escapes as a fatal `ExceptionInInitializerError`.

**HANG mechanism — hypothesis, unconfirmed.** No thread dump or further
diagnostic exists in these logs to show why the process then hangs rather
than surfacing this as an ordinary test failure/crash. Leading hypothesis:
the mis-typed exception breaks an expected internal catch/fallback in
`jnr-posix`'s provider selection (normally recoverable via a specific
exception type), turning what should be a benign fallback into an unhandled
fatal class-init failure whose unwinding then stalls — possibly a
class-initialization monitor for
`POSIXFactory$DefaultLibCProvider$SingletonHolder` never being
released/marked erroneous. Not traced far enough this session to confirm a
concrete deadlock mechanism.

**Documentation status:** the `ThrowNew`-discards-payload design itself was
noted generically in an old architecture review
(`docs/internal/reviews/fable-2026-06-10/fixes/vm-runtime-jni.md:43-45`,
`docs/internal/reviews/full-review-2026-06-20.md:648` — the latter
explicitly lists `Throw`/`ThrowNew` as only "spot-checked ... wiring", not
deep-read) but was never filed as an actionable bug with a concrete failing
symptom. No existing doc references `jni_throw_new`, `ThrowNew`,
`POSIXFactory`, or `jnr/posix`. Given `ThrowNew` is a generic JNI primitive
used far beyond `jnr-posix`, this is likely a **broader-impact bug** than
just this one class — any native library that relies on `ThrowNew`
signaling a *specific* catchable exception type will hit the same
substitution.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-cassandra` | `org.springframework.boot.cassandra.autoconfigure.CassandraAutoConfigurationTests` (HANG) |
