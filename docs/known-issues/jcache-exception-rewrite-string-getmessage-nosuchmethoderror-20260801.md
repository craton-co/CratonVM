# JCache exception-rewrite tests: `NoSuchMethodError: java.lang.String.getMessage()` — unmasked by the `VM.latestUserDefinedLoader0` fix

| | |
|---|---|
| **Status** | OPEN — newly surfaced, not root-caused. |
| **Category** | VM-CORRECTNESS (likely type-confusion / wrong receiver class after deserialization) |
| **Discovered** | 2026-08-01, full Spring Framework suite rerun, dev `0c481c85dc`, Azure host `20.83.144.174`, worktree `/data/data/wt-springsuite8b-20260726`, `apps/spring-suite-runner`, real JDK 25, jit-real. |
| **CratonVM** | FAIL — `java.lang.RuntimeException: java.lang.NoSuchMethodError: java.lang.String.getMessage()Ljava/lang/String;` |
| **HotSpot** | not independently re-verified this session |

## Context

Directly caused by (and only became visible after) the `VM.latestUserDefinedLoader0`
native-registration fix landing on `dev` (`70789969f3` /
`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md`).
Before that fix, every one of these 5 classes failed earlier and more
opaquely with `UnsatisfiedLinkError: jdk/internal/misc/VM.latestUserDefinedLoader0()`
(a missing native meant real Java deserialization couldn't proceed at all).
Now that deserialization can actually complete, these 5 classes get further
into the same test method and hit a **new, distinct** failure — a classic
"fixing one bug exposes the next one" case.

## Symptom

All 5 affected classes fail identically, in the identical test method:

```
FAILCAUSE org.springframework.cache.jcache.config.JCacheJavaConfigTests :: cacheExceptionRewriteCallStack() :: java.lang.RuntimeException: java.lang.NoSuchMethodError: java.lang.String.getMessage()Ljava/lang/String;
FAILCAUSE org.springframework.cache.jcache.config.JCacheStandaloneConfigTests :: cacheExceptionRewriteCallStack() :: java.lang.RuntimeException: java.lang.NoSuchMethodError: java.lang.String.getMessage()Ljava/lang/String;
FAILCAUSE org.springframework.cache.aspectj.JCacheAspectJNamespaceConfigTests :: cacheExceptionRewriteCallStack() :: java.lang.RuntimeException: java.lang.NoSuchMethodError: java.lang.String.getMessage()Ljava/lang/String;
FAILCAUSE org.springframework.cache.jcache.config.JCacheNamespaceDrivenTests :: cacheExceptionRewriteCallStack() :: java.lang.RuntimeException: java.lang.NoSuchMethodError: java.lang.String.getMessage()Ljava/lang/String;
FAILCAUSE org.springframework.cache.aspectj.JCacheAspectJJavaConfigTests :: cacheExceptionRewriteCallStack() :: java.lang.RuntimeException: java.lang.NoSuchMethodError: java.lang.String.getMessage()Ljava/lang/String;
```

`java.lang.String` has no `getMessage()` method — that's `Throwable`'s. A
`NoSuchMethodError` naming `String.getMessage()` means some receiver the VM
believes/resolves to be a `java.lang.String` had `.getMessage()` invoked on
it — i.e. a value that should be a `Throwable` (or subtype) is instead
typed/tagged as `String` at the call site, or vice versa: a real `String`
object ended up where a `Throwable` was expected.

## Test being exercised

`AbstractJCacheAnnotationTests.cacheExceptionRewriteCallStack()`
(`spring-context-support/src/testFixtures/java/org/springframework/contextsupport/testfixture/jcache/AbstractJCacheAnnotationTests.java:131`):

```java
protected void cacheExceptionRewriteCallStack() {
    long ref = service.exceptionInvocations();
    assertThatExceptionOfType(UnsupportedOperationException.class).isThrownBy(() ->
            service.cacheWithException(this.keyItem, true))
        .satisfies(first -> {
            assertThat(service.exceptionInvocations()).isEqualTo(ref + 1);
            UnsupportedOperationException second = methodInCallStack(this.keyItem);
            assertThat(service.exceptionInvocations()).isEqualTo(ref + 1);
            assertThat(first).hasCause(second.getCause());
            assertThat(first).hasMessage(second.getMessage());   // <-- line 146, the likely call site
            ...
        });
}

private UnsupportedOperationException methodInCallStack(String keyItem) {
    try {
        service.cacheWithException(keyItem, true);
        throw new IllegalStateException("Should have thrown an exception");
    }
    catch (UnsupportedOperationException e) {
        return e;
    }
}
```

`methodInCallStack` itself does no explicit serialization — it's a plain
try/catch. The connection to the `VM.latestUserDefinedLoader0` fix is
therefore almost certainly **indirect**, through JCache's own
`cacheWithException`/exception-caching implementation (JSR-107 cache
providers commonly serialize cached values, including cached exceptions,
for isolation) — not traced further this session. `second.getMessage()` at
line 146 is the most likely call site given it's the only `.getMessage()`
call in the method and `second` is statically typed `UnsupportedOperationException`.

## Not yet done

- Not root-caused to a specific Rust source line.
- Not confirmed whether the wrong-type value is the cached/deserialized
  exception itself, or something resolved from `NativeContext::
  frame_class_ids()`/the loader-walk machinery the `VM.latestUserDefinedLoader0`
  fix added (`latest_user_defined_loader_class` in
  `native-builtins/src/serialization.rs`) picking up a stale/wrong class id.
- Not verified against HotSpot in isolation (though by construction this
  can't be a HotSpot-shared defect — the test is standard JSR-107/Spring
  test code with no CratonVM-specific behavior implied).
- No standalone/minimal repro built yet — only observed via the full
  `apps/spring-suite-runner` suite run.

## Reproduce

```bash
cd apps/spring-suite-runner
CRATONVM_BIN=<path to cratonvm> ./run-suite.sh run --category all --jit on --jdk real \
  --only 'JCacheJavaConfigTests$|JCacheStandaloneConfigTests$|JCacheNamespaceDrivenTests$|JCacheAspectJNamespaceConfigTests$|JCacheAspectJJavaConfigTests$' \
  --batch 1
```

## Next steps for whoever continues

1. Get a live trace (`KRUN_STACK=1` or targeted logging) on
   `JCacheJavaConfigTests#cacheExceptionRewriteCallStack` to see exactly
   which receiver/value is mistyped as `String` at the failing call site.
2. Check whether `service.cacheWithException`'s JSR-107 cache
   implementation (used by all 5 affected configs — plain JCache config,
   namespace-driven config, and both AspectJ variants) round-trips the
   cached exception through Java serialization internally, and if so
   whether it's landing back in the loader-walk path the
   `VM.latestUserDefinedLoader0` fix touches.
3. Since this affects only these 5 classes (all sharing the same test
   fixture method) and not the other 56 classes the `VM.latestUserDefinedLoader0`
   fix newly unblocked, the defect is likely specific to this particular
   cache-value round-trip shape, not a general regression in the loader-walk
   fix itself.

## Related

[`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md`](../internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md)
— the fix that unmasked this.
