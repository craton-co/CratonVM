# `MergedAnnotationsTests` — reflection-obtained vs. re-synthesized annotation proxy class identity

Status: open

Date observed: 2026-07-03

## Summary

`org.springframework.core.annotation.MergedAnnotationsTests.synthesizedAnnotationShouldReuseJdkProxyClass()`
fails under CratonVM real-JDK+JIT mode (HotSpot passes; 177/178 other tests in
this class pass — this is the only failure in the class).

```java
Method method = WebController.class.getMethod("handleMappedWithValueAttribute");
RequestMapping jdkRequestMapping = method.getAnnotation(RequestMapping.class);
RequestMapping synthesizedRequestMapping = MergedAnnotation.from(jdkRequestMapping).synthesize();
...
assertThat(jdkRequestMapping.getClass()).isSameAs(synthesizedRequestMapping.getClass()); // FAILS
```

Observed: `jdkRequestMapping.getClass()` reports the annotation *interface*
type (`...MergedAnnotationsTests.RequestMapping`); `synthesizedRequestMapping.getClass()`
reports a real generated proxy class (`$Proxy4`). On HotSpot both report the
*same* `$ProxyN` class, because `java.lang.reflect.Proxy` caches generated
proxy classes by `(ClassLoader, ordered interface list)`, and Spring's
`SynthesizedMergedAnnotationInvocationHandler.createProxy()` requests the
exact same `(loader, [RequestMapping.class])` key that the JDK's own
annotation-parsing machinery used to build `jdkRequestMapping`.

## Root cause (confirmed by direct repro + code reading)

`method.getAnnotation(X)` is intercepted by CratonVM's
`native_method_get_annotation` (`native-builtins/src/lang_class.rs`), which
calls `create_annotation_proxy`. That function only wraps the result in a
*real* `$ProxyN` (via `wrap_annotation_in_real_proxy` →
`define_or_get_proxy_class`, the same cache `Proxy.newProxyInstance` uses)
when the env-gated `CRATONVM_REAL_ANNOTATIONS` feature is enabled — **default
off**. With it off (the default, and what the suite runner uses), reflection-
obtained annotation instances stay a bare synthetic `AnnotationProxy` whose
`getClass()` reports the annotation *type* (see
`vm/src/vm/vm_exec.rs::annotation_proxy_dispatch_impl`, the `"getClass"` arm,
which deliberately returns the annotation type mirror — a documented
trade-off, not an oversight).

Meanwhile Spring's `synthesize()` calls `Proxy.newProxyInstance()` explicitly
— real user code — which goes through `define_or_get_proxy_class`
(`native-builtins/src/lib.rs`), a **separate**, real-proxy-by-default
(`CRATONVM_REAL_PROXY`, default ON) code path, producing a genuine `$ProxyN`.

So the two annotation instances are represented by two fundamentally
different runtime shapes (bare synthetic object vs. real JDK proxy), and no
amount of cache-key fiddling on the `synthesize()` side can make
`jdkRequestMapping.getClass()` agree — the reflection side would need to
*also* become a real proxy.

### Why simply enabling `CRATONVM_REAL_ANNOTATIONS` is not a safe fix here

Verified directly: running with `CRATONVM_REAL_ANNOTATIONS=1` makes every
annotation instance (not just `RequestMapping`) a real proxy, and this
immediately breaks JUnit's own test discovery for this class with:

```
NullPointerException: Cannot invoke "Integer.intValue()" because the return
value of "Proxy$Dispatch.invokeProxy(...)" is null
  at $Proxy0.hashCode()
  at org.junit.platform.commons.util.AnnotationUtils.findMetaAnnotation
```

This is a second, independent, pre-existing bug: `native_proxy_dispatch_invoke`
(`native-builtins/src/lib.rs`) has a special case for an `AnnotationProxy`
handler that routes `Object`-inherited methods (`hashCode`/`equals`/`toString`)
by calling `ctx.invoke("java/lang/annotation/AnnotationProxy", &method_name, ...)`
directly by name — but the *actual* implementation of those semantics lives in
`annotation_proxy_dispatch_impl` (`vm/src/vm/vm_exec.rs`), a different,
non-by-name Rust function that the primary interpreter dispatch path
(`proxy_invoke_handler_shared`) calls directly. The by-name `ctx.invoke` path
used by the "2nd call site" fallback (documented in-code as reached when a
JIT-compiled call site bypasses the primary dispatch hook) doesn't resolve to
that logic and returns null. `CRATONVM_REAL_ANNOTATIONS` is explicitly
commented as gated off pending "wide soak" for exactly this class of reason.

Separately (found while tracing the identity question, independently real):
`wrap_annotation_in_real_proxy` hardcodes `loader_id = 0` when calling
`define_or_get_proxy_class`, instead of the annotation's actual defining
classloader namespace (contrast `native_proxy_new_instance`, which uses
`proxy_loader_namespace()`). Even with the dispatch bug above fixed, two
proxies for the same interface built through different loader-id values
would still land in different cache buckets. This is a real, narrow,
low-risk bug worth fixing independently of the default-enablement question
(dead code while the feature stays off) — not done in this pass to keep the
`core.*` change surface minimal.

## Why not fixed here

Making this one identity assertion pass requires either (a) enabling
`CRATONVM_REAL_ANNOTATIONS` broadly, which is independently broken (confirmed
above) and explicitly flagged elsewhere as needing a wide regression soak
across many suites — well beyond this bug-cluster's scope, or (b) a
narrower "lie in `getClass()` only" shim that would need to reach into
`native-builtins`'s proxy-class cache from `vm/src/vm/vm_exec.rs`, which are
one-directional-dependent crates (`vm` depends on `native-builtins`, and the
handful of existing precedents for `vm` calling into `native-builtins`, e.g.
`classloader_real::get_or_create_system_cl`, all require a live
`&mut dyn NativeContext`, which `annotation_proxy_dispatch_impl` does not
have — it only has `&SharedVm`). Doing this safely needs either threading a
`NativeContext` handle through, or duplicating class-definition logic
in the `vm` crate against the *same* shared cache — real surgery, not a
quick patch, and risks the shared GC-safety-sensitive proxy machinery used by
every other proxy consumer in the VM.

## Suggested next steps

1. Fix the narrow, independently-real bugs first (both are low-risk, additive):
   - `wrap_annotation_in_real_proxy`'s hardcoded `loader_id=0`
     (`native-builtins/src/lang_class.rs`).
   - `native_proxy_dispatch_invoke`'s `AnnotationProxy` by-name dispatch for
     `hashCode`/`equals`/`toString`/`getClass` (`native-builtins/src/lib.rs`) —
     route through the same logic `annotation_proxy_dispatch_impl` uses
     instead of `ctx.invoke(class, name, ...)`.
2. Soak `CRATONVM_REAL_ANNOTATIONS=1` broadly (not just this one class) with
   both fixes in place before considering flipping the default.
3. Only then would `synthesizedAnnotationShouldReuseJdkProxyClass` pass
   without a separate `getClass()`-only shim.
