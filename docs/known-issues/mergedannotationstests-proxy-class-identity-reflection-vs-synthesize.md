# `MergedAnnotationsTests` — reflection-obtained vs. re-synthesized annotation proxy class identity

Status: fixed (gated behind default-off `CRATONVM_REAL_ANNOTATIONS`); the two residual issues from the prior pass are now root-caused and fixed too. Default flip is still a separate, deliberate decision — see "Should the default flip?" below.

Date observed: 2026-07-03
Date updated: 2026-07-04

## Summary

`org.springframework.core.annotation.MergedAnnotationsTests.synthesizedAnnotationShouldReuseJdkProxyClass()`
failed under CratonVM real-JDK+JIT mode (HotSpot passes; 177/178 other tests in
this class pass — this was the only failure in the class, and remains the only
one at the **default** `CRATONVM_REAL_ANNOTATIONS=off` setting, which is
expected and correct — see below).

```java
Method method = WebController.class.getMethod("handleMappedWithValueAttribute");
RequestMapping jdkRequestMapping = method.getAnnotation(RequestMapping.class);
RequestMapping synthesizedRequestMapping = MergedAnnotation.from(jdkRequestMapping).synthesize();
...
assertThat(jdkRequestMapping.getClass()).isSameAs(synthesizedRequestMapping.getClass()); // FAILED at gate-off
```

`CRATONVM_REAL_ANNOTATIONS=1` now makes the **entire** `core.annotation.*`
package pass (728/728, minus one pre-existing unrelated `@Disabled`-style
skip) — see "Fixed" below for the full chain of bugs it took to get there.

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
break JUnit's own test discovery, so nobody had ever soaked it far enough to
find (let alone fix) the identity bug itself, or the several other bugs
documented below.

## Fixed (in two passes, 2026-07-03 and 2026-07-04)

All fixes below are gated behind `CRATONVM_REAL_ANNOTATIONS` (default OFF),
touch only dead code while it stays off, OR (fix 5) are behind a new
per-call option that defaults to today's behavior everywhere except
generated-proxy class definition. Verified: default settings
(`CRATONVM_REAL_ANNOTATIONS` unset) are byte-for-byte unchanged — 726/728
across the full `core.annotation.*` package (29 classes), identical to the
untouched baseline before any of this work, both for the single
`MergedAnnotationsTests` class (177/178) and the package as a whole.

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
   `MergedAnnotationClassLoaderTests.synthesizedUsesCorrectClassLoader`'s
   first assertion. Now registers the annotation's defining loader (when
   user-defined) against the generated `proxy_cid`, mirroring
   `native_proxy_new_instance` exactly.

3. **`native_proxy_dispatch_invoke`'s `AnnotationProxy` by-name dispatch for
   `hashCode`/`equals`/`toString`/`getClass`/`annotationType`/`getType`
   returned null or wrong values.** This is the "2nd call site" reached once
   the JIT has compiled a generated `$ProxyN`'s `hashCode`/`equals`/`toString`
   body (real bytecode that calls `Proxy$Dispatch.invokeProxy`), bypassing
   the primary interpreter dispatch hook (`proxy_invoke_handler_shared` →
   `annotation_proxy_dispatch_impl` in `vm/src/vm/vm_exec.rs`). The existing
   by-name fallback (`ctx.invoke("AnnotationProxy", mname,
   "()Ljava/lang/Object;", ...)`) does not reliably resolve these names
   through that logic. Confirmed via direct repro: this was the exact cause
   of `NullPointerException: Cannot invoke "Integer.intValue()" because the
   return value of "Proxy$Dispatch.invokeProxy(...)" is null` at
   `$Proxy0.hashCode()`, which broke JUnit's own test discovery
   (`AnnotationUtils.findMetaAnnotation`) the moment
   `CRATONVM_REAL_ANNOTATIONS=1` was set.

   `annotationType`/`getType` were added in the second pass: Spring's own
   meta-annotation introspection machinery (`AnnotationTypeMappings`,
   `TypeMappedAnnotation`) calls `annotationType()` repeatedly — exactly the
   kind of hot, JIT-compiled call that hits this "2nd call site" — and was a
   contributing factor to residual issue B below before it was root-caused as
   a separate, deeper bug (see fix 5).

   Fix: `native-builtins/src/lang_class.rs` gained self-contained
   `ctx_annotation_proxy_hash_code` / `ctx_annotation_proxy_equals` /
   `ctx_annotation_proxy_to_string` helpers built on `NativeContext`,
   mirroring `annotation_proxy_hash_code` / `annotation_proxy_equals` /
   `annotation_proxy_to_string` in `vm/src/vm/vm_exec.rs`. They are
   duplicated rather than shared because `native-builtins` cannot call into
   `vm` (the crate dependency only goes the other way — `vm` depends on
   `native-builtins`), and threading a `NativeContext` into the `vm`-side
   functions would mean touching the primary proxy-dispatch hot path used by
   every proxy consumer in the VM (real surgery, out of scope — see
   "Why the deeper identity-shim path was still not taken" below).
   `native_proxy_dispatch_invoke` now calls these directly for the six
   Object-inherited/annotation-identity names instead of going through
   `ctx.invoke`.

   Two additional wrinkles surfaced only by running the actual generated
   bytecode (found via direct debugging, not visible from code reading
   alone) — **both are instances of the same pitfall, hit twice**:

   - **Boxing.** `invokeProxy`'s declared return type is `Object`, and the
     generated method body does `CHECKCAST Integer; Integer.intValue()`
     (`hashCode`) / `CHECKCAST Boolean; Boolean.booleanValue()` (`equals`).
     Returning a raw `Value::Int`/`Value::Int(0/1)` — which is what
     `annotation_proxy_dispatch_impl`'s own `hashCode`/`equals` arms do,
     correctly, from the *primary* dispatch path where the ultimate consumer
     expects an unboxed primitive directly — is not a valid object reference
     here and silently becomes null. Fixed by boxing through the existing
     `box_value(ctx, Value::Int(..), "I"/"Z")` helper. The exact same mistake
     recurred in the fix-5 delegation call below (`other.equals(proxy)` is
     ALSO a primitive-`Z`-returning bytecode call reached via `ctx.invoke`)
     and caused a `checkcast: not an object reference` VM abend in
     `MergedAnnotationsTests` before it, too, was boxed.
   - **Equals-argument unwrapping.** Under `CRATONVM_REAL_ANNOTATIONS=1`
     *every* annotation instance is a real `$ProxyN`, so the argument to
     `.equals(other)` is typically *also* a real proxy, not a bare
     `AnnotationProxy` — unlike the historical case (gate off) where only one
     side could ever be a real proxy. Without unwrapping `other` to its
     `AnnotationProxy` handler (slot 0) first, every same-type comparison
     between two now-real-proxied annotations spuriously compared unequal.
     Fixed by mirroring the equivalent unwrap already present in
     `proxy_invoke_handler_shared` (`vm/src/vm/vm_exec.rs`, lines ~8119–8143).

4. **`ctx_annotation_proxy_equals` had no cross-type delegation** — fixed
   residual issue A below.

5. **General `CRATONVM_REAL_PROXY` bug: generated `$ProxyN` classes link
   `implements <Iface>` against the WRONG same-named class when the
   interface was loaded by a non-default `ClassLoader`.** Root-caused residual
   issue B below; this is the one that actually mattered most, and is not
   annotation-specific at all.

## Residual issues from the first pass — now root-caused and fixed

### A. `equalsForSynthesizedAnnotations` — cross-type equals delegation

**Root cause:** `assertThat(reflectivelyObtained).isEqualTo(springSynthesized)`
compares two annotation instances of the SAME type but with fundamentally
different `InvocationHandler`s: the reflectively-obtained side's handler is
our `AnnotationProxy`; Spring's `synthesize()`-built side's handler is
Spring's own `SynthesizedMergedAnnotationInvocationHandler` (real Java
bytecode, not our `AnnotationProxy`). `ctx_annotation_proxy_equals`'s
same-type structural comparison can only ever recognize another
`AnnotationProxy`-backed instance, so it always returned `false` for a
genuinely different `Annotation` implementation — exactly the case this test
exercises (`MergedAnnotationsTests.java:2096-2097`).

**Fix:** mirrored the delegation `annotation_proxy_invoke_shared` already
does in `vm/src/vm/vm_exec.rs` (lines ~8548-8583): when `other` is neither our
`AnnotationProxy` nor a real proxy wrapping one, check whether `other`'s
class implements the SAME annotation interface (`ctx.is_subclass(other_cid,
ann_cid)` against the type mirror in `ANN_PROXY_TYPE_MIRROR`); if so, delegate
to `other.equals(proxy)` — its own `equals`, whatever implementation it is,
presumably knows how to compare member values reflectively against any
`Annotation`, the same way HotSpot's `AnnotationInvocationHandler.equals`
does. Only a genuinely non-annotation-typed `other` returns `false` directly.

Hit the exact same boxing pitfall as fix 3 above: the delegated
`ctx.invoke(..., "equals", "(Ljava/lang/Object;)Z", ...)` call returns a raw
`Value::Int` for the primitive `Z` return, which needed boxing via
`box_value(ctx, flag, "Z")` before returning it as `native_proxy_dispatch_invoke`'s
own `Object`-typed result — missing this caused a VM abend
(`internal error: checkcast: not an object reference`) that briefly regressed
`MergedAnnotationsTests` to a hard crash before it was caught and fixed in
the same pass.

### B. Interface-linkage bug: "`$ProxyN` must be an instance of interface X"

**This was never annotation-specific.** Isolated with a minimal, Spring-free,
JUnit-free repro (`ProxyLoaderTest.java`, no longer in the tree — was scratch
work under `.claude/worktrees/annoproxy-residual/scratch_repro/`, reproducible
from the description below): a plain

```java
ClassLoader loader = new MyLoader(parent);          // any ClassLoader subclass that defineClass()es its own copy
Class<?> iface = loader.loadClass("Greeter");        // interface loaded by `loader`, NOT the app loader
Object proxy = Proxy.newProxyInstance(loader, new Class<?>[]{iface}, handler);
iface.isInstance(proxy);                              // false! should be true
Method.invoke(iface.getMethod("greet"), proxy);        // IllegalArgumentException: object of type $ProxyN is not an instance of Greeter
```

reproduces the identical failure with **zero** Spring, JUnit, or annotation
code involved, confirming this is a general bug in dynamic-proxy class
generation under a custom `ClassLoader` — affecting `CRATONVM_REAL_PROXY`
(default **ON**) everywhere, not just the `CRATONVM_REAL_ANNOTATIONS` path.
It happened to surface in the annotation work because
`AnnotationIntrospectionFailureTests`/`MergedAnnotationClassLoaderTests` are
exactly the two `core.annotation.*` tests that load an interface through a
non-default `ClassLoader` (`OverridingClassLoader` subclasses).

**Root cause, precisely:** `define_or_get_proxy_class` generates the `$ProxyN`
classfile with `implements <IfaceName>` encoded as a plain constant-pool
`CONSTANT_Class` **name string** (classfiles have no other way to reference a
type) and defines it via `ClassManager::define_class_with_options` under the
SAME `ClassLoaderId::UserDefined(loader_namespace)` the interface itself was
loaded under. During linking, `define_class_with_options`'s `resolve_supertype`
closure resolves that name — and here's the bug: the loader-scoped lookup
that would find the interface's ALREADY-LOADED `ClassId` under that exact
namespace (`loaded_classes_probe`) is gated behind
`CRATONVM_LOADER_AWARE_RESOLUTION` (`classloading/src/class_manager.rs:116-131`,
**default OFF**, added for an unrelated Hibernate bytecode-enhancement
scenario — "link an enhanced subclass to its same-loader supertype copy").
With that gate off (the default), resolution always falls through to the
loader-*agnostic* global `this.load_class(internal)`, which binds to
whichever same-named class it finds first — typically a DIFFERENT class
already loaded by the application loader (e.g. because the interface's own
`.class` literal got eagerly resolved earlier by the enclosing class's
loader). The generated `$ProxyN` ends up implementing THAT unrelated
same-named class, not the interface it was actually built for — confirmed
directly via a debug trace showing two distinct `ClassId`s for the identical
name under the identical loader namespace.

Verified via `CRATONVM_DBG_PROXY=1`/an added `CRATONVM_DBG_ISINSTANCE=1` trace
in `native_class_is_instance` (`native-builtins/src/lang_class.rs`) printing
the generated proxy's actual resolved `class_interfaces()` next to the
target `ClassId` being checked — they were both named `Greeter` but were
different `ClassId`s.

**Fix (surgical, not a global default flip):** added
`force_loader_faithful_linking: bool` (default `false`) to
`DefineClassOptions` (`classloading/src/class_manager.rs`) and to the
FFI-facing `DefineClassFull` (`native-api/src/registry.rs`), threaded through
`vm/src/vm/vm_exec.rs`'s `define_class_full`. `resolve_supertype` now checks
`loader_aware_resolution() || options.force_loader_faithful_linking`.
`native-builtins/src/lib.rs`'s `define_or_get_proxy_class` sets this to
`true` unconditionally for every generated proxy class — unlike the
ambiguous "which same-loader copy is more correct" question the global gate
exists for, a generated proxy has exactly one correct answer: it MUST link
against the exact interface `ClassId` its generator resolved. This is
byte-identical for every OTHER class-definition call site (the new field
defaults `false`), so the global `CRATONVM_LOADER_AWARE_RESOLUTION` default
is untouched.

Note this fix applies to **every** `Proxy.newProxyInstance`/`getProxyClass`
call, not just annotations — it is *not* gated behind
`CRATONVM_REAL_ANNOTATIONS` and takes effect under the default-on
`CRATONVM_REAL_PROXY` too. This is intentional: it is a strict correctness
fix with no plausible downside (the loader-scoped lookup, when it doesn't
find an exact match, still falls through to the existing global
`load_class`), and was re-verified against the full `core.annotation.*`
default-gate-off baseline (726/728, unchanged) plus a `spring-aop` sanity
pass (60/66 classes clean; the 6 non-clean classes' failures/timeouts all
independently traced to a missing test-classpath dependency
(`org/springframework/aot/test/generate/TestGenerationContext`, pre-existing,
unrelated) or abstract test base classes with no runnable methods — none
touch dynamic-proxy or classloading code).

## Verified effect (after all fixes)

`core.annotation.*` package (29 classes, 728 test methods):

| | gate off (default) | gate on (`CRATONVM_REAL_ANNOTATIONS=1`) |
|---|---|---|
| passed | 726/728 | 727/728 |
| failing | 1 (`synthesizedAnnotationShouldReuseJdkProxyClass`) | 0 |
| skipped | 0 | 1 (pre-existing, unrelated `@Disabled`-style skip) |

Gate-off is byte-for-byte identical to the untouched baseline — zero
regression. Gate-on now passes the **entire** package cleanly.

A separate, unrelated JIT bug (`a11025aa2`'s invokedynamic/`needs_heap`
regression — see `docs/known-issues/jit-sigsegv-regression-20260704.md` and
`jit-nativecall-dispatch-sigsegv-annotation-scanning.md`) was briefly
suspected of undermining this result — a JIT SIGSEGV surfaced in 5 of these
29 classes under a larger combined test batch, reproducible even under
completely default settings with none of this doc's changes involved. That
bug is now fixed upstream and confirmed resolved (the same 5-class repro and
the full package both re-verified clean, 727/728 and 726/728 respectively,
after picking up the fix). Not a bug in this doc's fixes at any point —
confirmed via `--nojit`, which was always clean.

## Why the deeper identity-shim path was still not taken

A narrower "lie in `getClass()` only" shim that doesn't require
`CRATONVM_REAL_ANNOTATIONS` at all would need to reach into
`native-builtins`'s proxy-class cache from `vm/src/vm/vm_exec.rs`, which are
one-directional-dependent crates (`vm` depends on `native-builtins`, and the
handful of existing precedents for `vm` calling into `native-builtins`, e.g.
`classloader_real::get_or_create_system_cl`, all require a live
`&mut dyn NativeContext`, which `annotation_proxy_dispatch_impl` does not
have — it only has `&SharedVm`). This remains true and remains out of scope
— it's moot now that the `CRATONVM_REAL_ANNOTATIONS` path itself works
end-to-end.

## Should the default flip?

Not decided here — flagging what's now true so whoever makes that call has
current information:

- The ORIGINAL blocker (JUnit discovery crash) is gone.
- The full `core.annotation.*` package now passes identically well under
  either setting.
- Fix 5 (proxy interface linking) is the biggest-blast-radius change in this
  set — it changes behavior for `CRATONVM_REAL_PROXY` broadly, independent of
  `CRATONVM_REAL_ANNOTATIONS`, and got a `spring-aop` sanity pass but not a
  full-suite soak.
- `CRATONVM_REAL_ANNOTATIONS` itself reshapes the runtime representation of
  *every* annotation instance in the VM — a wide blast radius the original
  gate comment explicitly called out as needing "wide soak" before
  default-on, and that soak (beyond this one package) has not happened.

Recommend running the fuller spring-core/spring-context/spring-beans suites
(annotation-heavy, e.g. bean-definition metadata scanning) under
`CRATONVM_REAL_ANNOTATIONS=1` before considering the default flip — this pass
only re-confirmed `core.annotation.*` plus a partial `spring-aop` pass.
