# RESOLVED: `Collections.sort`/`Arrays.sort` lambda-implemented `Comparable`

**Status: RESOLVED 2026-07-15**

## Resolution

Hidden lambda-proxy classes are intentionally absent from `class_manager`.
The natural-order native paths now resolve a proxy's functional interface via
`lambda_functional_interface` before walking its super-interfaces, so a
lambda implementing an interface that extends `Comparable` passes the same
preflight as an ordinary object. Diagnostics also synthesize the proxy's
host-based lambda name instead of degrading to `<unknown>`.

The sort merge follows the JDK's right-vs-left natural-order probe. This keeps
ordinary Comparable behavior unchanged while honoring concrete precedence
when the opposing lambda inherits a zero-returning interface default method.

Validation on the final isolated release executable:

- `ObservationHandlerGroupsTests`: 2/2 PASS with JIT on and off.
- `TracingAndMeterObservationHandlerGroupTests`: 4/4 PASS with JIT on and off.
- `cargo test --release -p cratonvm-native-collections --test mock_arraylist arrays_sort_`: 2/2 PASS.

## Symptom

Two independent test classes fail identically with a `ClassCastException`
whose message contains the literal string `<unknown>` instead of a real
class name — itself a symptom worth noting, since a genuine HotSpot
`ClassCastException` always names the concrete offending class:

```
module/spring-boot-micrometer-observation:
  JUnit Jupiter:ObservationHandlerGroupsTests:shouldGroupCategoriesIntoFirstMatchingHandlerAndRespectCategoryOrder()
    => java.lang.ClassCastException: element of class <unknown> does not implement java.lang.Comparable
       org.springframework.boot.micrometer.observation.autoconfigure.ObservationHandlerGroups.sort(ObservationHandlerGroups.java:50)
       org.springframework.boot.micrometer.observation.autoconfigure.ObservationHandlerGroups.<init>(ObservationHandlerGroups.java:45)
       org.springframework.boot.micrometer.observation.autoconfigure.ObservationHandlerGroupsTests.shouldGroupCategoriesIntoFirstMatchingHandlerAndRespectCategoryOrder(ObservationHandlerGroupsTests.java:46)

module/spring-boot-micrometer-tracing:
  JUnit Jupiter:TracingAndMeterObservationHandlerGroupTests:compareToSortsBeforeMeterObservationHandlerGroup()
    => java.lang.ClassCastException: element of class <unknown> does not implement java.lang.Comparable
       org.springframework.boot.micrometer.tracing.autoconfigure.TracingAndMeterObservationHandlerGroupTests.sort(TracingAndMeterObservationHandlerGroupTests.java:111)
       org.springframework.boot.micrometer.tracing.autoconfigure.TracingAndMeterObservationHandlerGroupTests.compareToSortsBeforeMeterObservationHandlerGroup(TracingAndMeterObservationHandlerGroupTests.java:53)
```

Logs: `apps\spring-boot-suite-runner\.suite\results\crashfail-20260714\shard7\logs\module_spring-boot-micrometer-observation.org.springframework.boot.micrometer.obser-5b5bb04579f3.out.log`
and `...module_spring-boot-micrometer-tracing.org.springframework.boot.micrometer.tracing.a-eae7bf680f11.out.log`
(only shard7 of the 8 shards contains either class).

Both failures trace back to `Collections.sort(list)` called on a
`List<ObservationHandlerGroup>`. `ObservationHandlerGroup` is:

```java
// apps/spring-boot/module/spring-boot-micrometer-observation/src/main/java/.../ObservationHandlerGroup.java
public interface ObservationHandlerGroup extends Comparable<ObservationHandlerGroup> {
    ...
    @Override
    default int compareTo(ObservationHandlerGroup other) {
        return 0;
    }
    ...
    static <H extends ObservationHandler<?>> ObservationHandlerGroup of(Class<H> handlerType) {
        Assert.notNull(handlerType, "'handlerType' must not be null");
        return () -> handlerType;   // <-- a LAMBDA implementing ObservationHandlerGroup
    }
}
```

Both failing tests construct at least one group via
`ObservationHandlerGroup.of(SomeHandlerType.class)`, i.e. `() -> handlerType`
— a **lambda** (compiled via `invokedynamic`/`LambdaMetafactory`) whose
single implemented interface, `ObservationHandlerGroup`, extends
`Comparable<ObservationHandlerGroup>` with a default `compareTo`. Sorting a
list containing this lambda instance is exactly the scenario that trips the
bug: the object genuinely does implement `Comparable` (transitively, via its
functional interface), but CratonVM's native sort path says it doesn't, and
also can't print its class name.

## Root cause

This is **not** heap/object-identity corruption — the lambda instance is
intact and dispatches correctly elsewhere (e.g. its `handlerType()`/SAM
method call, and `compareTo` itself, would work fine if reached). It's a
known category of CratonVM gap: **lambda-proxy classes are never registered
in `class_manager`**, they live in a separate `shared.lambda_proxies` map
(`vm/src/vm/vm_exec.rs`, `vm/src/vm.rs`), and a handful of native code paths
still consult only `class_manager`-backed accessors, silently treating a
lambda instance as an object of a completely unknown, interface-less class.

Concretely:

1. `Collections.sort(List)` and `Arrays.sort(Object[])`'s native
   implementations both verify `Comparable` before sorting, via
   `implements_comparable()`:
   `native-collections/src/lib.rs:9222-9236`
   ```rust
   fn implements_comparable(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
       let mut cid = ctx.class_id_of_object(obj);
       for _ in 0..64 {
           for iface in ctx.class_interfaces(cid) {
               if iface_extends_comparable(ctx, iface) { return true; }
           }
           match ctx.superclass_of(cid) { Some(p) => cid = p, None => break }
       }
       false
   }
   ```
   The two call sites are the `Collections.sort` native
   (`native-collections/src/lib.rs:9579-9595`) and `Arrays.sort(Object[])`
   (`native-collections/src/lib.rs:9191-9207`) — both build the
   `ClassCastException` message the same way.

2. `ctx.class_interfaces(cid)` (`vm/src/vm/vm_exec.rs:6195-6200`) is:
   ```rust
   fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId> {
       let cm = self.shared.class_manager.read();
       cm.get_class(class_id).map(|c| c.interfaces.clone()).unwrap_or_default()
   }
   ```
   For a lambda-proxy `ClassId` (allocated via `shared.alloc_lambda_proxy_id()`
   and tracked only in `shared.lambda_proxies`, never inserted into
   `class_manager`), `get_class(class_id)` returns `None`, so this silently
   returns an **empty** interface list — `implements_comparable` therefore
   never sees `ObservationHandlerGroup` (or its `Comparable` supertype) and
   returns `false`.

3. The `<unknown>` text comes from the exact same class-manager miss, one
   step later, when building the exception message
   (`native-collections/src/lib.rs:9195-9197` and `:9583-9585`):
   ```rust
   let cname = ctx.class_name_of_id(ctx.class_id_of_object(*obj))
       .unwrap_or_else(|| "<unknown>".to_string());
   ```
   `ctx.class_name_of_id` (`vm/src/vm/vm_exec.rs:2859-2865`) also only
   queries `class_manager`, so it returns `None` for the same lambda-proxy
   `ClassId`, and the fallback placeholder fires. **`"<unknown>"` is an
   intentional, documented fallback string** (see
   `vm/src/vm/vm_exec.rs:10985-10991`, "returns `"<unknown>"` to give a
   non-empty…"), not a formatting bug in isolation — but it is only ever
   *supposed* to fire for genuinely unresolvable/foreign identities, not for
   a live, well-formed lambda instance. Its appearance here is the visible
   symptom of the deeper interface-resolution gap in step 2.

4. **This exact class of gap has already been found and fixed once**, in a
   sibling code path — see `docs/internal/spring-aop-lambda-hidden-class-pointcut-matching-RESOLVED.md`.
   That bug was `Class.getGenericInterfaces()` (and `getPackageName()`)
   returning empty/wrong for lambda-proxy classes, root-caused to exactly
   this "lambda proxies aren't in `class_manager`" gap, and fixed by adding
   a lambda-proxy special case that resolves the SAM interface via
   `ctx.lambda_functional_interface(class_id)` — an accessor that already
   exists for precisely this purpose:
   - Trait method: `native-api/src/registry.rs:1342` (default `None`)
   - Real impl: `vm/src/vm/vm_exec.rs:4471-4477`, backed by
     `shared.lambda_proxies.read().get(&class_id).map(|cs| cs.functional_interface...)`
   - Already used as the fix pattern in
     `native-builtins/src/lang_class.rs:2712,8960,11927,12079`
     (`native_class_get_interfaces`, `native_class_get_generic_interfaces`, etc.)

   `native-collections`'s `implements_comparable`/`iface_extends_comparable`
   (`native-collections/src/lib.rs:9222-9258`) is one of the call sites that
   was **not** updated with this special case — it still only walks
   `ctx.class_interfaces`/`ctx.superclass_of`/`ctx.class_name_of_id`, all of
   which are blind to lambda-proxy class ids. The likely fix shape (not
   applied here, per instructions — documentation only): in
   `implements_comparable`, before/alongside the `class_interfaces` walk,
   check `ctx.lambda_functional_interface(cid)`; if present, resolve that
   interface name to a `ClassId` via `ctx.class_id_by_name(...)` and feed it
   into the existing `iface_extends_comparable` walk (mirroring how
   `native_class_get_interfaces` treats the SAM interface as `[iface]`).
   The `cname` fallback in the `ClassCastException` message should
   similarly prefer `ctx.lambda_functional_interface`/`ctx.lambda_proxy_host`
   over the bare `class_name_of_id` miss so a real (if synthetic) name is
   reported instead of `<unknown>` even in genuine non-Comparable-lambda
   cases.

**Verdict: this is a genuine, narrowly-scoped CratonVM native-dispatch bug**
(missing lambda-proxy special case in one specific Comparable-verification
path), not a Spring Boot / Micrometer application bug, and not evidence of
heap/object-identity corruption — the object's identity and layout are
fine; only two class-manager-backed lookups (`class_interfaces`,
`class_name_of_id`) are structurally unable to see lambda-proxy classes,
exactly as previously diagnosed for `getGenericInterfaces()`/
`getPackageName()`.

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -Vm craton -Category all -Jit on -Parallel 1 -RunName comparable-lambda-repro-20260714 `
  -SpringBootRoot C:\craton\CratonVM\apps\spring-boot
```

then filter/run just the two affected classes (per
`apps\spring-boot-suite-runner\run-spring-boot-suite.md`, use `-ListOnly`
against `all-tests.tsv` to find their row indices, or run a `-Start`/`-Count`
slice covering `module/spring-boot-micrometer-observation` and
`module/spring-boot-micrometer-tracing`):

- `module/spring-boot-micrometer-observation` →
  `org.springframework.boot.micrometer.observation.autoconfigure.ObservationHandlerGroupsTests`
- `module/spring-boot-micrometer-tracing` →
  `org.springframework.boot.micrometer.tracing.autoconfigure.TracingAndMeterObservationHandlerGroupTests`

Both fail with `--jit on` (default); not yet cross-checked with `-Jit off`
(the bug is in a shared native-dispatch helper, not the JIT, so it is
expected to reproduce identically with `--nojit`, but this has not been
verified).

## Related

- `docs/internal/spring-aop-lambda-hidden-class-pointcut-matching-RESOLVED.md`
  — the same "lambda proxy classes aren't registered in `class_manager`"
  root cause, previously found and fixed for
  `Class.getGenericInterfaces()`/`getPackageName()` via the
  `ctx.lambda_functional_interface(class_id)` special case. This doc's bug
  is the same gap surfacing in `native-collections`'s `Comparable`
  verification for `Collections.sort`/`Arrays.sort`, which never received
  the equivalent special case.
- Not part of the descriptor-coercion / slot-reuse / heap-corruption bug
  family (`reference_descriptor_coercion_slot_reuse_trap`,
  `reference_overlay_real_class_corruption`) despite the superficially
  similar "unknown/garbage class identity" presentation — those involve
  actual corrupted or misread object headers; here the object and its
  `ClassId` are both well-formed, they simply index into the wrong registry
  (`lambda_proxies` instead of `class_manager`), and every accessor that
  only knows about `class_manager` degrades gracefully-but-wrongly to
  empty/`None` instead of falling through to `lambda_proxies`.
