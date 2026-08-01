# `Throwable.getCause()` returned the message `String` — JCache exception-rewrite + Servlet handler-exception failures

| | |
|---|---|
| **Status** | ✅ FIXED 2026-08-01 — root-caused, minimal repro built, all 6 affected classes green. |
| **Category** | VM-CORRECTNESS (`Throwable` native field access — a synthetic-layout fallback fired on a real-JDK receiver) |
| **Discovered** | 2026-08-01, full Spring Framework suite rerun, dev `0c481c85dc`, Azure host `20.83.144.174` |
| **Fixed in** | branch `fix/jcache-getmessage-20260801`, off dev `36451a613f` |
| **HotSpot** | PASS — baseline captured this session with the same probes |

## Symptom

Two distinct-looking failure shapes, one root cause. Five JCache classes:

```
FAILCAUSE org.springframework.cache.jcache.config.JCacheJavaConfigTests :: cacheExceptionRewriteCallStack() :: java.lang.RuntimeException: java.lang.NoSuchMethodError: java.lang.String.getMessage()Ljava/lang/String;
FAILCAUSE org.springframework.cache.jcache.config.JCacheStandaloneConfigTests :: ... (same)
FAILCAUSE org.springframework.cache.jcache.config.JCacheNamespaceDrivenTests :: ... (same)
FAILCAUSE org.springframework.cache.aspectj.JCacheAspectJNamespaceConfigTests :: ... (same)
FAILCAUSE org.springframework.cache.aspectj.JCacheAspectJJavaConfigTests :: ... (same)
```

…and one Servlet class the original report did not connect to it, found by re-scanning
the same full-suite `failcauses.log` set for the defect's signature (any `Throwable`
method dispatched on a `java.lang.String`):

```
FAILCAUSE org.springframework.web.servlet.mvc.method.annotation.ServletAnnotationControllerHandlerMethodTests :: [1] true :: jakarta.servlet.ServletException: Handler processing failed: java.lang.NoSuchMethodError: java.lang.String.getCause()Ljava/lang/Throwable;
FAILCAUSE ... :: [2] false :: (same)
```

## Root cause

`KRUN_STACK=1` named the call site immediately — and it is **not** the
`second.getMessage()` at `AbstractJCacheAnnotationTests.java:146` the original report
guessed. It is line **145**, `assertThat(first).hasCause(second.getCause())`:

```
Caused by: java.lang.NoSuchMethodError: java.lang.String.getMessage()Ljava/lang/String;
	at org.assertj.core.error.ShouldHaveCause.<init>(ShouldHaveCause.java:67)
	at org.assertj.core.error.ShouldHaveCause.shouldHaveCause(ShouldHaveCause.java:29)
	at org.assertj.core.internal.Throwables.assertHasCause(Throwables.java:101)
	at org.assertj.core.api.AbstractThrowableAssert.hasCause(AbstractThrowableAssert.java:125)
	at ...AbstractJCacheAnnotationTests.lambda$cacheExceptionRewriteCallStack$1(...:145)
```

`second.getCause()` returned a **`java.lang.String`**. `getCause()` is declared to
return `Throwable`, so nothing checkcasts the result; AssertJ took the mismatch branch
and, while formatting the failure message, called `.getMessage()` on it. The Servlet
class is the same defect one step further along: Spring's handler-exception path walks
the cause chain, so it called `.getCause()` on the returned `String` instead.

The offending value is the exception's own `detailMessage`, delivered by a layout
fallback that must never have fired:

```rust
// native-builtins/src/lang_misc.rs — BEFORE
fn read_throwable_field(ctx, this, field_name) -> Value {
    let by_name = ctx.get_field_by_name(this, field_name);
    if !matches!(by_name, Value::Object(None)) { return by_name; }
    if let Some(slot) = synthetic_throwable_slot(field_name) {   // "cause" => 1
        if slot < ctx.object_num_fields(this) { return ctx.get_field(this, slot); }
    }
    by_name
}
```

`synthetic_throwable_slot` exists for **synthetic** `Throwable` stubs, which have no
field names at all, and its own doc-comment claimed it "is consulted ONLY when the
by-name lookup fails, so a real receiver is untouched". That is not what the code
tested. `get_field_by_name` answers `Object(None)` for **two** different situations:
"this class has no such field" (synthetic stub → the slot layout is the only way to
read it) *and* "this real-JDK field genuinely holds null". The second case fell
through to the fallback, and slot 1 of the real-JDK `Throwable` layout is
`detailMessage`:

| slot | 0 | 1 | 2 | 3 | 4 | 5 |
|---|---|---|---|---|---|---|
| real JDK 25 `Throwable` | `backtrace` | `detailMessage` | `cause` | `stackTrace` | `depth` | `suppressedExceptions` |
| `synthetic_throwable_slot` | `detailMessage` | `cause` | `suppressedExceptions` | — | — | — |

So `getCause()` on any real-JDK `Throwable` whose `cause` is genuinely null returned
the message `String`. The same fallback also aliased `suppressedExceptions` (synthetic
slot 2) onto the real layout's `cause`, so such a throwable reported its cause as its
suppressed list.

`native_throwable_get_cause` already carried a comment describing exactly this failure
("slot 1 of a real-JDK Throwable layout is `detailMessage`, so the fallback returned
the message text as the cause … `NoSuchMethodError: java/lang/String.getCause()`").
That earlier fix deleted the fallback from `native_throwable_get_cause`'s own tail —
but the same fallback lives in `read_throwable_field`, which is the *first* line of
`native_throwable_get_cause`, so it never took effect.

### Why only these classes, and why only after the `latestUserDefinedLoader0` fix

Freshly-constructed throwables were mostly unaffected: the JDK declares
`private Throwable cause = this;`, and CratonVM's exception-constructor natives mirror
that sentinel — a non-null read, so the fallback never ran. A throwable that comes back
from **deserialization** never runs a constructor: `ObjectInputStream` writes the fields
directly, so `cause` is a plain null and the fallback fired.

`CacheResultInterceptor.rewriteCallStack` (spring-context-support) clones the cached
exception with `SerializationUtils.clone(exception)` before rethrowing it — a full
`ObjectOutputStream`/`ObjectInputStream` round-trip. That is the only Spring JCache path
that deserializes a `Throwable`, which is why exactly the five classes sharing
`AbstractJCacheAnnotationTests.cacheExceptionRewriteCallStack()` were hit. Before
`VM.latestUserDefinedLoader0` was registered (`70789969f3`) the round-trip died earlier
with `UnsatisfiedLinkError`, so the defect was unreachable. The loader-walk fix is
**not** implicated — `frame_class_ids()` / `latest_user_defined_loader_class` behave
correctly here; they only unblocked the path.

### Second defect found the same way

`register_exception_extras_natives` (`native-builtins/src/lib.rs`) registers
`native_exception_init_empty` / `native_exception_init_msg` for ~56 exception classes
and — being registered later — wins the registry slot over
`lang_misc::native_exc_init_noargs` / `native_exc_init_message`. The `lang_misc` pair
mirrors the JDK's `cause = this` initializer; the `lib.rs` pair did not:

```
new RuntimeException()                    -> cause == this    (correct; not in that list)
new UnsupportedOperationException("MSG")  -> cause unwritten  (wrong)
```

Observable three ways: through reflection, through real-JDK `initCause()` bytecode
(`cause != this` ⇒ "Can't overwrite cause"), and in the serialized form. The serialized
streams for `new UnsupportedOperationException("MSG")` made it plain — HotSpot emits the
self back-reference `q ~ 00 02`, CratonVM emitted `p` (`TC_NULL`):

```
HotSpot   ...L..suppressedExceptionst..Ljava/util/List;xp q.~.. t..MSG ...   668 bytes
before    ...L..suppressedExceptionst..Ljava/util/List;xp p     t..MSG ...   659 bytes
after     ...L..suppressedExceptionst..Ljava/util/List;xp q.~.. t..MSG ...   663 bytes
```

(The residual 5-byte gap is unrelated: `StackTraceElement.classLoaderName` is null under
CratonVM where HotSpot writes `"app"`. Not part of this bug.)

## Fix

1. **`native-builtins/src/lang_misc.rs` — `read_throwable_field`.** Before consulting
   `synthetic_throwable_slot`, ask whether the receiver's class actually
   declares/inherits the name (`resolve_field_index_by_class_id`). A real-JDK receiver
   now returns its genuine null; only a nameless synthetic stub reaches the slot layout.
   This is the fix for the reported failures.
2. **`native-builtins/src/lib.rs` — `native_exception_init_empty` /
   `native_exception_init_msg`.** Mirror the JDK `private Throwable cause = this;`
   initializer, exactly as their `lang_misc` twins already did, so the ~56 classes those
   natives shadow stop diverging from HotSpot.

## Reproduce

Minimal repro, no Spring and no JCache (`CauseProbe.java`):

```java
UnsupportedOperationException fresh = new UnsupportedOperationException("Test exception");
Throwable clone = (Throwable) new ObjectInputStream(new ByteArrayInputStream(ser(fresh))).readObject();
System.out.println(clone.getCause());   // HotSpot: null   CratonVM before: "Test exception" (a String)
```

Suite classes:

```bash
cd apps/spring-suite-runner
CRATONVM_BIN=<vm> KRUN_STACK=1 ./one.sh org.springframework.cache.jcache.config.JCacheJavaConfigTests
```

## Verification performed

| check | before | after |
|---|---|---|
| `JCacheJavaConfigTests` | 31/32 FAIL | **32/32 OK** |
| `JCacheStandaloneConfigTests` | FAIL | **28/28 OK** |
| `JCacheNamespaceDrivenTests` | FAIL | **30/30 OK** |
| `JCacheAspectJNamespaceConfigTests` | FAIL | **28/28 OK** |
| `JCacheAspectJJavaConfigTests` | 27/28 FAIL | **28/28 OK** |
| `ServletAnnotationControllerHandlerMethodTests` | 237/241 FAIL | **241/241 OK** |
| `CauseProbe` / `SerRepro` / `SerBytes`, real JDK | diverged from HotSpot | **matches HotSpot**, incl. the serialized self back-reference |
| `ThrowProbe` — 9 `Throwable`-surface cases, real JDK | == HotSpot | **== HotSpot, unchanged** |
| `IteProbe` — `InvocationTargetException` / `printStackTrace` / suppressed | baseline | **identical to baseline** |
| `ThrowProbe` under `--synthetic-jdk` | pristine-dev control | **identical to control** |
| `cargo check --workspace --all-targets`, default and `--features synthetic-jdk` | — | **0 errors both** |
| `cargo test -p cratonvm-vm --lib --features synthetic-jdk` (blocking gate) | — | **3852 passed / 0 failed** |
| `cargo test -p cratonvm-native-builtins --lib`, default / `synthetic-jdk` | — | 3202/2 and 3377/2 — the **same 2** (`panama::tests::test_85_4_upcall_handle_and_invoke`, `tls_deny::tests::every_plaintext_base_overload_is_accounted_for`) fail identically on a pristine `origin/dev` worktree, so pre-existing |

### Spring-suite A/B, shard 1/6 (474 classes), jit-real, same slice both sides

| | pre-fix binary | post-fix binary |
|---|---|---|
| OK | 466 | **468** |
| FAIL | 8 | 6 |

Deltas, class by class:

- `JCacheJavaConfigTests`, `JCacheAspectJNamespaceConfigTests` — FAIL → **OK** (this fix).
- `ErrorHandlerIntegrationTests` — FAIL → OK, but the pre-side failcause is
  `ResourceAccessException: localhost:34657 failed to respond` on the Jetty
  parameterisation. A port/network flake, not this fix.
- `RetryInterceptorTests` — OK → FAIL, `withEnableAnnotation()`. **Not a regression.**
  The failure is `Expecting … java.io.IOException but was: java.lang.IllegalStateException`
  at `RetryInterceptorTests.java:617`, i.e. the `@ConcurrencyLimit(1)` throttle let two
  threads into `retryOperation()` at once so the bean's own
  `current.incrementAndGet() > 1` guard fired. That is the already-documented
  `ConcurrencyThrottleSupport` flake, and it reproduces identically on the **pre-fix**
  binary. Measured by re-running the class in isolation, alternating binaries run by
  run so load drift hits both sides equally: **pre-fix 4/40 failures, post-fix 5/40** —
  indistinguishable. (Two earlier non-interleaved batches, run while the box was
  otherwise busy, gave 3/50 and 8/50; the interleaved figure is the trustworthy one.)
- The remaining 5 failures are identical on both sides.


## Related

- [`../h2-suite-bugs/bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md`](../h2-suite-bugs/bug-h2-missing-native-latestuserdefinedloader0-serialization-FIXED.md)
  — the fix that unmasked this. Not itself at fault.
- [`../throwable-addsuppressed-clobbers-cause-FIXED.md`](../throwable-addsuppressed-clobbers-cause-FIXED.md)
  — same family: a hardcoded `Throwable` slot index aliasing onto `cause`.

## Noted, not changed

`NativeContext::get_field_by_name` reads the raw slot, while `get_field(index)` routes
through the descriptor-aware `get_field_as`. An object-reference slot that was never
written therefore reads back as `Value::Int(0)` through the by-name accessor and
`Value::Object(None)` through the indexed one — the two disagree about the same field
of the same object. `init_suppressed_sentinel` already documents relying on this. It is
what made `read_throwable_field`'s `Object(None)` test unreliable in the first place,
but making `get_field_by_name` descriptor-aware changes behaviour for every native in
the tree, so it is recorded here as a separate item rather than folded into this fix.
