# `WebMvcEndpointIntegrationTests`: the annotations vanished, not a condition — FIXED 2026-08-29

**Fixed in `f62216ca0`.** The two failures in this class were one defect, and
it is not in the autoconfiguration chain at all: a cached annotation proxy
outlived the generated `$ProxyN` class it is an instance of, so
`getDeclaredAnnotations()` threw and Spring silently dropped **every**
annotation on nine autoconfiguration classes.

Retires the `WebMvcEndpointIntegrationTests` half of
`known-issues/springboot/jettyreactive-tls-timeout-and-webmvcendpoint-autoconfig-20260829.md`.
The Jetty mTLS half of that page is a different defect and stays open — see
`retired/jettyreactive-mtls-verify-timeout-is-load-not-a-close-path-defect-RETIRED-20260901.md`.

## What the page said, and what the log said

The known-issues page recorded the symptom correctly —

```
webMvcEndpointHandlerMappingIsConfiguredWithPathPatternParser():
  NoSuchBeanDefinitionException: No qualifying bean of type
  'org.springframework.boot.webmvc...WebMvcEndpointHandlerMapping' available
endpointJsonMapperCanBeApplied():
  AssertionFailedError: [HTTP status code] expected: 200 but was: 404
```

— and named the next step as "diff which `@Conditional*` evaluates
differently under CratonVM". **No condition evaluates differently.** The
suite run's own preserved log had already recorded the cause, 280 lines above
the failure:

```
04:46:09.674 [main] INFO org.springframework.core.annotation.MergedAnnotation --
  Failed to introspect annotations on class
  org.springframework.boot.autoconfigure.context.PropertyPlaceholderAutoConfiguration:
  java.lang.NoClassDefFoundError: jdk/proxy1/$Proxy26
```

repeated for `DispatcherServletAutoConfiguration`, `WebMvcAutoConfiguration`,
`EndpointAutoConfiguration`, `WebEndpointAutoConfiguration`,
`JacksonAutoConfiguration`, `HttpMessageConvertersAutoConfiguration`,
`ServletManagementContextAutoConfiguration` and
`ManagementContextAutoConfiguration` — nine classes, on **every** context
refresh from the second one onward.

`AnnotationsScanner` catches that `Throwable` and returns **no annotations**.
With no annotations there is no `@AutoConfiguration`, no `@Conditional`, no
`@Bean` — the classes are inert, `WebMvcEndpointHandlerMapping` is never
defined, and the two failures follow: `getBean` throws, and the unrouted
actuator endpoint answers 404 instead of 200. The conditions were never
consulted, so a condition report would have shown nothing.

Read the diagnostic that already ran before building one: `grep`ping the
preserved `.out.log` for `NoClassDefFoundError` was the whole diagnosis.

## Root cause

`lang_class::ANNOTATION_PROXY_CACHE` maps
`(holder class id, annotation descriptor)` → the cached annotation-proxy
**instance**, and it is itself a GC root (`gc_scan_annotation_proxy_roots`,
`gc_update_annotation_proxy_refs`). Nothing purged it except VM teardown
(`forget_vm_annotation_proxies`).

An annotation proxy is an instance of a generated `jdk/proxyN/$ProxyM` whose
defining loader is the annotation "container" loader — for Spring, one of the
per-context isolation loaders. When a test's application context is discarded
that loader becomes unreachable, and `ClassManager::unload_user_classes` (the
GC-driven path) retires the generated class with it.

The cached instance survives that, **because it is rooted in its own right**.
Its class does not. The two halves are rooted by different machinery and only
one of them was reconciled. Every later `getDeclaredAnnotations()` on that
holder is then handed an instance of an unloaded class, and resolving it
throws `NoClassDefFoundError: jdk/proxy1/$Proxy26`. Nothing recovers, because
the poisoned row is never evicted — which is exactly why the log shows the
same nine classes failing on every subsequent refresh rather than once.

This is the same failure family, one level down, as the generated-`$ProxyN`
**class** cache: `PROXY_CLASS_CACHE` already has
`forget_unloaded_proxy_classes`, added when `BatchJdbcAutoConfigurationTests`,
`FreeMarkerAutoConfigurationReactiveIntegrationTests` and
`OpenTelemetrySdkAutoConfigurationTests` failed under `-XX:+UseZGC` for the
class-side version of it. The instance-side cache was never given the
matching purge.

## The fix

`lang_class::forget_unloaded_annotation_proxies(vm_identity, dead, class_id_of)`
drops every row whose cached proxy's class **or** whose holder class was just
unloaded, and releases that proxy's child roots (the member-value roots keyed
by proxy address, which would otherwise root garbage for the life of the
process).

Called from `vm/src/memory/gc.rs` immediately after
`forget_unloaded_proxy_classes`, on the same only-if-something-was-actually-
unloaded path, so a collection that unloads nothing does not churn a valid
cache. Dropping a row is always safe: the next `getDeclaredAnnotations()`
rebuilds the proxy.

The key side is purged on the same evidence as the class cache's: a row keyed
on a retired holder can only be matched again through class-id reuse, and
`class_manager`'s own array-cache comment records that reuse as real — "a
loader id is never recycled, but a *class* id under a live loader is".

6 unit tests in `lang_class::annotation_proxy_cache_unload_tests` cover the
value side (the row this exists for: holder alive, proxy's class unloaded),
the key side, an unresolvable ref, a live row surviving, an empty dead set
consulting nothing, and cross-VM isolation.

## Verification

`WebMvcEndpointIntegrationTests`, `dev` + this fix, Windows:

| arm | runs | failures |
|---|---|---|
| default collector | 4 | 0 |
| `-XX:+UseG1GC` | 4 | 0 |
| `-XX:+UseGenerationalGC` | 4 | 0 |
| both failing methods individually, ×30 each | 60 | 0 |
| full class, 3 concurrent × 10 CPU spinners | 18 | 0 |
| `--nojit` | 3 | 0 |

Workspace `cargo test --lib` unchanged. Note `test_classes/*.class` is
gitignored, so a fresh worktree fails to *compile* `cratonvm-reader`'s lib
tests until those fixtures are generated — not a regression.

## What this does NOT prove

The test does not reproduce on demand. It failed once, in the 2026-08-29
full-suite run; **16 runs on the same Linux host, same fixture and the same
binary file that produced it** pass, as do 89 Windows runs. The defect is
real and the mechanism is proven from the failing run's own log, but the
trigger needs a collection that unloads the container loader between two
context refreshes, and that does not happen on most runs. Treat a future
green as consistent-with-fixed, not as the fix being re-demonstrated.

A direct probe (`AnnProxyStress`: introspect → drop → `System.gc()` →
re-introspect, ×150) is byte-identical on both VMs and does **not** reach the
defect — plain `System.gc()` does not unload the container loader. Enumerating
the `ManagementContextConfiguration.imports` resources 200× is identical too
(3 URLs, 6 candidates, both VMs), which refutes the dropped-`.imports`
hypothesis that the missing-bean symptom otherwise invites.

## Method note

The suite run this came from printed, in its own header:

```
hotspot baseline: none at .../hotspot-baseline-latest.tsv
  -- every failure will be attributed to CratonVM
```

Both of that run's residuals were filed as "confirmed CratonVM-specific". One
of them is (this page). Run the baseline arm the runner tells you is missing.
