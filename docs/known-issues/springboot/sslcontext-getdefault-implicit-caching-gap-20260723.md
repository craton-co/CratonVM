# `SSLContext.getDefault()` allocates a fresh context every call unless `setDefault()` was explicitly invoked first — breaks JDK singleton-default contract

**Status: OPEN — found 2026-07-23 (craton-rerun-20260723), root cause confirmed at file:line precision**

## Symptom

`module/spring-boot-micrometer-metrics`,
`OtlpMetricsExportAutoConfigurationTests.whenNoSslBundleDefaultHttpSenderHasDefaultSslContext()`:

```
java.lang.AssertionError:
Expecting actual:
  javax.net.ssl.SSLContext@7a9d6
and:
  javax.net.ssl.SSLContext@7a9dd
to refer to the same object
       org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.OtlpMetricsExportAutoConfigurationTests.lambda$whenNoSslBundleDefaultHttpSenderHasDefaultSslContext$0(OtlpMetricsExportAutoConfigurationTests.java:222)
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard7/logs/module_spring-boot-micrometer-metrics.org.springframework.boot.micrometer.metrics.autoconfigur-01a2497c6bfb.out.log`
(20/21 other tests in the class pass).

Test source
(`apps/spring-boot/module/spring-boot-micrometer-metrics/src/test/java/org/springframework/boot/micrometer/metrics/autoconfigure/export/otlp/OtlpMetricsExportAutoConfigurationTests.java:217-224`):

```java
@Test
void whenNoSslBundleDefaultHttpSenderHasDefaultSslContext() {
    this.contextRunner.withUserConfiguration(BaseConfiguration.class).run((context) -> {
        assertThat(context).hasSingleBean(OtlpHttpMetricsSender.class);
        OtlpHttpMetricsSender metricsSender = context.getBean(OtlpHttpMetricsSender.class);
        HttpClient httpClient = extractHttpClient(metricsSender);
        assertThat(httpClient.sslContext()).isSameAs(SSLContext.getDefault());
    });
}
```

No SSL bundle is configured for this test, so auto-configuration builds the
`java.net.http.HttpClient` with whatever the JDK considers *the* default
`SSLContext` (`HttpClient.Builder` uses `SSLContext.getDefault()` internally
when none is set). The test then calls `SSLContext.getDefault()` again and
asserts referential (`isSameAs`) equality with what the `HttpClient` already
holds — this is only true if `SSLContext.getDefault()` is a cached
singleton, which the real JDK guarantees (`SSLContext` caches the lazily-created
default the first time it's needed, `setDefault()` or not).

## Root cause — CONFIRMED at file:line precision

`native-builtins/src/net_phase_e.rs`, the live `SSLContext.getDefault()`
registration (`register_re6_ssl_context`, ~line 9406-9440):

```rust
r.register(
    ctx_cls,
    "getDefault",
    "()Ljavax/net/ssl/SSLContext;",
    |ctx, _args| {
        // FIX (es-restclientbuilder-ssl-default-context-20260710): if
        // `SSLContext.setDefault(ctx)` installed a context ...
        if let Some(ctx_obj) = crate::t27_tls::get_runtime_default_ssl_context() {
            return Ok(Some(Value::Object(Some(ctx_obj))));
        }
        let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLContext", 2);
        let name = ctx.create_string("TLS");
        ctx.set_field(obj, 0, Value::Object(Some(name)));
        ctx.set_field(obj, 1, Value::Int(1));
        Ok(Some(Value::Object(Some(obj))))
    },
);
```

This is a real, previously-fixed bug
([`elasticsearch-restclientbuilder-ssl-default-context-20260710-FIXED.md`](../../internal/fixed-suite-bugs/elasticsearch-restclientbuilder-ssl-default-context-20260710.md))
— but that fix only covers the **explicit** path: it caches whatever object
was passed to `SSLContext.setDefault(ctx)` (via
`crate::t27_tls::set_runtime_default_ssl_context`/`get_runtime_default_ssl_context`,
a `Mutex<Option<ObjectRef>>` behind a `OnceLock`). Nobody in this test's
path ever calls `setDefault()`. When `get_runtime_default_ssl_context()`
returns `None` (the slot was never populated), `getDefault()` falls through
to the `let obj = alloc_concurrent_synthetic(...)` branch and **allocates a
brand-new synthetic `SSLContext` object on every single call** — there is no
caching of the *implicit* default at all. Real `javax.net.ssl.SSLContext`
lazily creates its default exactly once (a static field guarded the first
time any caller needs it, independent of whether `setDefault()` was ever
invoked) and returns that same instance forever after. CratonVM's version
only ever caches the object a caller explicitly handed to `setDefault()`;
the "nobody called setDefault, synthesize one" branch was apparently never
updated to populate the same cache slot on its own first use.

Two calls to `SSLContext.getDefault()` with no intervening `setDefault()`
(exactly this test's shape: one implicit call inside `HttpClient.Builder`
during bean construction, one explicit call in the test assertion) therefore
produce two distinct `alloc_concurrent_synthetic` objects — reference
inequality, matching the observed `AssertionError` precisely.

## Suggested fix (not applied — this session is documentation-only)

In the `None` branch of `getDefault()`, populate
`crate::t27_tls::set_runtime_default_ssl_context(obj)` (the same slot the
explicit-`setDefault()` path already uses) with the freshly-allocated
synthetic context **before** returning it, so the very next `getDefault()`
call (explicit or implicit) hits the existing `Some(ctx_obj)` cache-hit
branch instead of allocating again. This mirrors real `SSLContext`'s
lazy-once-then-cached semantics exactly and reuses the GC-root-scanning
wiring (`vm/src/memory/roots.rs`/`gc.rs`) already in place for the explicit
path — no new storage needed.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-micrometer-metrics` | `org.springframework.boot.micrometer.metrics.autoconfigure.export.otlp.OtlpMetricsExportAutoConfigurationTests` (`whenNoSslBundleDefaultHttpSenderHasDefaultSslContext`, 1/21) |

Only 1 class in this session's batch, but the bug is general (any code
comparing two independent `SSLContext.getDefault()` calls, or caching one
`getDefault()` result and later comparing it against a fresh call, would hit
the same gap) — worth a broader classpath grep (`SSLContext.getDefault()`
call-site density) if picked up for a fix.
