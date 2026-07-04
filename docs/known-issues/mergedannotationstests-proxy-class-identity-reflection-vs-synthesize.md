# `MergedAnnotationsTests` — reflection-obtained vs. re-synthesized annotation proxy class identity

Status: primary bug fixed (gated, default-off); two independent follow-on bugs found during verification, not yet fixed

Date observed: 2026-07-03
Date updated: 2026-07-03

## Summary

`org.springframework.core.annotation.MergedAnnotationsTests.synthesizedAnnotationShouldReuseJdkProxyClass()`
failed under CratonVM real-JDK+JIT mode (HotSpot passes; 177/178 other tests in
this class pass — this was the only failure in the class, and remains the only
one at the **default** `CRATONVM_REAL_ANNOTATIONS=off` setting).

```java
Method method = WebController.class.getMethod("handleMappedWithValueAttribute");
RequestMapping jdkRequestMapping = method.getAnnotation(RequestMapping.class);
RequestMapping synthesizedRequestMapping = MergedAnnotation.from(jdkRequestMapping).synthesize();
...
assertThat(jdkRequestMapping.getClass()).isSameAs(synthesizedRequestMapping.getClass()); // FAILED
```

`CRATONVM_REAL_ANNOTATIONS=1` makes this test (and the whole class) pass — see
"Fixed in this pass" below — but the feature stays **default-off** because two
*other*, independent bugs were found while soak-testing it broadly (see
"Residual issues" below). Landing the fixes below is safe regardless: they are
either unconditionally correct or dead code while the gate stays off.

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

So the two annotation instances were represented by two fundamentally
different runtime shapes (bare synthetic object vs. real JDK proxy) unless
`CRATONVM_REAL_ANNOTATIONS` is on — and turning it on used to immediately
break JUnit's own test discovery (see below), so it never got soaked far
enough to fix the identity assertion itself.

## Fixed in this pass (native-builtins/src/lang_class.rs, native-builtins/src/lib.rs)

All fixes below are gated behind `CRATONVM_REAL_ANNOTATIONS` (default OFF) or
are unconditionally-correct dead-code-when-off fixes. Verified: default
settings (`CRATONVM_REAL_ANNOTATIONS` unset) are byte-for-byte unchanged —
726/728 across the full `core.annotation.*` package (29 classes), identical
to the pre-fix baseline, both for the single `MergedAnnotationsTests` class
(177/178) and the package as a whole.

1. **`wrap_annotation_in_real_proxy` hardcoded `loader_id=0`.** Now resolves
   the annotation type's actual defining classloader (via
   `native_class_get_class_loader`) and passes its `proxy_loader_namespace()`
   value to `define_or_get_proxy_class`, exactly mirroring
   `native_proxy_new_instance`'s pattern — so a `synthesize()`-built proxy and
   our reflection-obtained one land in the *same* cache bucket for the same
   `(loader, [interface])` key, which is required for `getClass()` identity
   to ever match.

2. **`wrap_annotation_in_real_proxy` never registered the generated proxy's
   defining loader.** Even with the namespace fixed above,
   `proxyClass.getClassLoader()` still fell back to the app-loader default
   (`defining_loader_for` returns `None`) because nothing called
   `register_defining_loader` for the newly-generated class — unlike
   `native_proxy_new_instance`, which does. This broke
   `MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader`
   (discovered during soak — see below). Now registers the annotation's
   defining loader (when user-defined) against the generated `proxy_cid`,
   mirroring `native_proxy_new_instance` exactly.

3. **`native_proxy_dispatch_invoke`'s `AnnotationProxy` by-name dispatch for
   `hashCode`/`equals`/`toString`/`getClass` returned null.** This is the
   "2nd call site" reached once the JIT has compiled a generated `$ProxyN`'s
   `hashCode`/`equals`/`toString` body (real bytecode that calls
   `Proxy$Dispatch.invokeProxy`), bypassing the primary interpreter dispatch
   hook (`proxy_invoke_handler_shared` → `annotation_proxy_dispatch_impl` in
   `vm/src/vm/vm_exec.rs`). The existing by-name fallback
   (`ctx.invoke("AnnotationProxy", mname, "()Ljava/lang/Object;", ...)`) does
   not reliably resolve these four names through that logic. Confirmed via
   direct repro: this was the exact cause of
   `NullPointerException: Cannot invoke "Integer.intValue()" because the
   return value of "Proxy$Dispatch.invokeProxy(...)" is null` at
   `$Proxy0.hashCode()`, which broke JUnit's own test discovery
   (`AnnotationUtils.findMetaAnnotation`) the moment
   `CRATONVM_REAL_ANNOTATIONS=1` was set — explaining why the feature never
   got soaked far enough to reach the identity bug at all.

   Fix: `native-builtins/src/lang_class.rs` gained self-contained
   `ctx_annotation_proxy_hash_code` / `ctx_annotation_proxy_equals` /
   `ctx_annotation_proxy_to_string` helpers built on `NativeContext`,
   mirroring `annotation_proxy_hash_code` / `annotation_proxy_equals` /
   `annotation_proxy_to_string` in `vm/src/vm/vm_exec.rs`. They are
   duplicated rather than shared because `native-builtins` cannot call into
   `vm` (the crate dependency only goes the other way — `vm` depends on
   `native-builtins`), and threading a `NativeContext` into the `vm`-side
   functions would mean touching the primary proxy-dispatch hot path used by
   every proxy consumer in the VM (real surgery, out of scope here — see
   "Why the deeper identity-shim path was still not taken" below, which still
   applies). `native_proxy_dispatch_invoke` now calls these directly for the
   four Object-inherited names instead of going through `ctx.invoke`.

   Two additional wrinkles surfaced only by running the actual generated
   bytecode (found via direct debugging, not visible from code reading
   alone):

   - **Boxing.** `invokeProxy`'s declared return type is `Object`, and the
     generated method body does `CHECKCAST Integer; Integer.intValue()`
     (`hashCode`) / `CHECKCAST Boolean; Boolean.booleanValue()` (`equals`).
     Returning a raw `Value::Int`/`Value::Int(0/1)` — which is what
     `annotation_proxy_dispatch_impl`'s own `hashCode`/`equals` arms do,
     correctly, from the *primary* dispatch path where the ultimate consumer
     expects an unboxed primitive directly — is not a valid object reference
     here and got silently treated as null. Fixed by boxing through the
     existing `box_value(ctx, Value::Int(..), "I"/"Z")` helper
     (`native-builtins/src/lang_class.rs`), the same one
     `native_proxy_new_instance`'s callers rely on elsewhere.
   - **Equals-argument unwrapping.** Under `CRATONVM_REAL_ANNOTATIONS=1`
     *every* annotation instance is a real `$ProxyN`, so the argument to
     `.equals(other)` is typically *also* a real proxy, not a bare
     `AnnotationProxy` — unlike the historical case (gate off) where only one
     side could ever be a real proxy. Without unwrapping `other` to its
     `AnnotationProxy` handler (slot 0) first, every same-type comparison
     between two now-real-proxied annotations spuriously compared unequal.
     Fixed by mirroring the equivalent unwrap already present in
     `proxy_invoke_handler_shared` (`vm/src/vm/vm_exec.rs`, lines ~8119–8143)
     before delegating to `ctx_annotation_proxy_equals`.

## Verified effect

With all of the above: `CRATONVM_REAL_ANNOTATIONS=1` against
`MergedAnnotationsTests` alone went from **EMPTY** (JUnit discovery crash) →
**178 found, 177 passed, 1 failed** — the *same* 177/178 that gate-off
produces, except the failing test is now different: the original target
(`synthesizedAnnotationShouldReuseJdkProxyClass`) now **passes**, and
`equalsForSynthesizedAnnotations` (previously masked by the discovery crash)
is the new sole failure — see "Residual issues" below.

Against the full `core.annotation.*` package (29 classes, 728 test methods):

| | gate off (default) | gate on (`CRATONVM_REAL_ANNOTATIONS=1`) |
|---|---|---|
| passed | 726/728 | 721/728 |
| failing classes | 1 (`MergedAnnotationsTests`) | 3 |

Gate-off numbers are byte-for-byte identical before and after this pass — no
regression. Gate-on trades the 1 known failure for 3 classes' worth of
*newly visible* failures (6 test methods) that were previously masked by the
discovery crash — see below.

## Residual issues (found during broad soak, NOT fixed in this pass)

Both of the following are independent of the fixes above and were **not
visible before** because the JUnit discovery NPE aborted the whole class
before any of this code ran. They are genuinely new discoveries, not
regressions caused by this pass's changes (confirmed: they persist whether or
not the classloader-registration fix, item 2 above, is applied — that fix
only moves `MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader`
past its *first* assertion into the second issue below).

### A. `equalsForSynthesizedAnnotations` — toString-format / equals mismatch

`assertThat(reflectivelyObtained).isEqualTo(springSynthesized)` fails. The
AssertJ failure message shows the *same underlying attribute values*
rendered in two different `toString()` styles — `(byte)0xff`,
`{...}`-bracketed arrays (Spring/modern-JDK `Annotation.toString()` style, on
the side going through Spring's own `SynthesizedMergedAnnotationInvocationHandler`)
vs. `-1`, `[...]`-bracketed arrays (our `ctx_annotation_proxy_to_string`'s
older format, ported from `annotation_proxy_to_string` in vm_exec.rs). The
differing display is *cosmetic* evidence of the real defect — `.equals()`
itself returns `false` — not yet root-caused. Candidates: a residual gap in
`ctx_annotation_proxy_equals` (which intentionally does not implement the
cross-type delegation that `annotation_proxy_invoke_shared` layers on top of
`annotation_proxy_dispatch_impl` — see the code comment on
`ctx_annotation_proxy_equals`), or a member-value representation mismatch
between the two sides not covered by the current comparison logic.

### B. Interface-linkage bug: "`$ProxyN` must be an instance of interface X"

Affects `AnnotationIntrospectionFailureTests` (all 4 methods) and the tail of
`MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader` (once
fix 2 above gets it past its classloader-identity assertion). `Method.invoke`
on a generated `$ProxyN` throws `IllegalArgumentException: Object of class
[$ProxyN] must be an instance of interface X`, i.e. the generated proxy class
does not actually implement the interface (`ExampleAnnotation`,
`ExampleMetaAnnotation`, `TestAnnotation`) that a `Method` was resolved
against. This message format is the real-JDK reflection wording (distinct
from CratonVM's own `native_method_invoke` IAE message,
`"object of type X is not an instance of Y"`, ruled out by grep), so it's
happening on a call path that runs real JDK bytecode, not our native.

This looks like a **general bug in the real-proxy-class-generation path**
(`define_or_get_proxy_class` / `classloading/src/proxy_gen.rs`), not something
specific to annotations — the common thread across the affected tests is a
nested/meta-annotation or custom-classloader-loaded interface, not anything
`wrap_annotation_in_real_proxy`-specific. It plausibly also affects the
broader `CRATONVM_REAL_PROXY` feature (default ON) outside of annotations,
which would make it independently worth investigating regardless of
`CRATONVM_REAL_ANNOTATIONS`'s fate. Not root-caused in this pass — flagged
here for follow-up.

## Why the deeper identity-shim path was still not taken

(Unchanged from before this pass.) A narrower "lie in `getClass()` only" shim
that doesn't require `CRATONVM_REAL_ANNOTATIONS` at all would need to reach
into `native-builtins`'s proxy-class cache from `vm/src/vm/vm_exec.rs`, which
are one-directional-dependent crates (`vm` depends on `native-builtins`, and
the handful of existing precedents for `vm` calling into `native-builtins`,
e.g. `classloader_real::get_or_create_system_cl`, all require a live
`&mut dyn NativeContext`, which `annotation_proxy_dispatch_impl` does not
have — it only has `&SharedVm`). Doing this safely needs either threading a
`NativeContext` handle through, or duplicating class-definition logic in the
`vm` crate against the *same* shared cache — real surgery, not a quick patch,
and risks the shared GC-safety-sensitive proxy machinery used by every other
proxy consumer in the VM. The fixes in this pass sidestep that by making the
*existing* `CRATONVM_REAL_ANNOTATIONS` path (which already avoids that
problem, since it's an env-gated whole-representation switch, not a
`getClass()`-only lie) actually functional up to the point of the two
residual issues above.

## Suggested next steps

1. Root-cause residual issue B (the interface-linkage bug) first — it looks
   broader than annotations and may be masking other `CRATONVM_REAL_PROXY`
   bugs unrelated to this cluster entirely.
2. Root-cause residual issue A (`equalsForSynthesizedAnnotations`).
3. Once both are fixed, re-soak `CRATONVM_REAL_ANNOTATIONS=1` broadly
   (`core.annotation.*` plus other annotation-heavy spring-core/spring-context
   suites) before considering flipping the default. Do not flip the default
   without that soak — this pass's package-level run alone (721/728, 3 failing
   classes) is not sufficient evidence.
