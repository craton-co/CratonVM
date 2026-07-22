# `Collections.singletonMap(...)` returns a `HashMap`-backed object instead of the real `Collections$SingletonMap` — `EnvironmentEndpointTests`

**Status: FIXED — 2026-07-18**

## Symptom

```
JUnit Jupiter:EnvironmentEndpointTests:propertyWithComplexTypeShouldNotFail()
  => org.opentest4j.AssertionFailedError:
expected: "Complex property type java.util.Collections$SingletonMap"
 but was: "Complex property type java.util.HashMap"
     org.springframework.boot.actuate.env.EnvironmentEndpointTests.propertyWithComplexTypeShouldNotFail(EnvironmentEndpointTests.java:240)
```

Full log: `apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-actuator.org.springframework.boot.actuate.env.EnvironmentEndpointTests.out.log`

The test (`EnvironmentEndpointTests.java:229-241`) puts
`Collections.singletonMap("bar", "baz")` into a property source, then asserts
that `EnvironmentEndpoint`'s "complex type" fallback describes its runtime
class by name. On CratonVM the reported class is `java.util.HashMap`; on
HotSpot it is the real `java.util.Collections$SingletonMap`.

## Root cause (confirmed against source)

`Collections.singletonMap(K, V)` is natively backed in both CratonVM
registration paths, and **both** allocate a plain `java.util.HashMap`
object rather than the real `Collections$SingletonMap`:

- `native-collections/src/lib.rs:38905-38918`
  (`native_collections_singleton_map`, registered at
  `native-collections/src/lib.rs:38484-38489` for
  `Collections.singletonMap(Object,Object)Ljava/util/Map;`):
  ```rust
  fn native_collections_singleton_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
      let key = args.first().cloned().unwrap_or(Value::Object(None));
      let val = args.get(1).cloned().unwrap_or(Value::Object(None));
      // Keep singletonMap native-backed for the same bootstrap reason as
      // singleton Set: WildFly calls simple Map methods before the real
      // Collections$SingletonMap wrapper's method surface is fully bridged.
      let map = alloc_backing_map(ctx);
      native_map_init(ctx, &[Value::Object(Some(map))])?;
      native_map_put(ctx, &[Value::Object(Some(map)), key, val])?;
      Ok(Some(Value::Object(Some(map))))
  }
  ```
  where `alloc_backing_map` (`native-collections/src/lib.rs:8265-8275`)
  literally allocates `java/util/HashMap`:
  ```rust
  fn alloc_backing_map(ctx: &mut dyn NativeContext) -> ObjectRef {
      let cid = ... ctx.ensure_class_initialized("java/util/HashMap") ...;
      let n = std::cmp::max(total, MAP_NUM_FIELDS);
      ctx.alloc_object(cid, n)
  }
  ```
- `native-builtins/src/phases_early.rs:384-409`
  (`native_collections_singleton_map`, the synthetic-JDK-mode twin) does the
  same thing by hand: `alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)`.

The returned object's runtime class is therefore genuinely
`java.util.HashMap` — `getClass().getName()` on it cannot report anything
else — which is exactly the observed `"java.util.HashMap"` vs. expected
`"java.util.Collections$SingletonMap"`.

By contrast, `Collections.singletonList(...)` in the same file **does**
allocate the real JDK class (`native-collections/src/lib.rs:10509-10512`:
`alloc_real_jdk(ctx, "java/util/Collections$SingletonList")`), so
`getClass()` on a singleton list is already correct — only the `Map`
sibling was left on the HashMap-backed synthetic path.

## Relationship to an existing FIXED doc — direct contradiction

[`../../internal/spring-boot-probe-sweep/SBR-12-getclass-interface-abstract-leak.md`](SBR-12-getclass-interface-abstract-leak.md),
"Follow-up (2026-06-23) — real-JDK singletons + interface-native
delegation", explicitly claims:

> `Collections.singletonList`/`singleton`/`singletonMap` previously returned
> synthetic `ArrayList`/`HashSet`/`HashMap` (wrong class **and** wrongly
> mutable). Per the "real JDK by default" principle they now return
> **real** `Collections$Singleton{List,,Map}` objects...
> Verified byte-identical to HotSpot jdk-25 for the whole singleton family.

Current source directly contradicts this for `singletonMap` specifically:
the comment right above the `HashMap`-backed implementation
(`native-collections/src/lib.rs:38911-38913`) is dated after that "FIXED"
claim and *explicitly, deliberately* re-introduces the HashMap-backed
behavior "for the same bootstrap reason as singleton Set... WildFly calls
simple Map methods before the real `Collections$SingletonMap` wrapper's
method surface is fully bridged." This reads as an intentional
WildFly-bootstrap trade-off added on top of the SBR-12 fix that silently
regressed the exact case that fix's own regression test suite covered — not
a fresh, unrelated bug. `Collections.singleton(Object)` (the `Set` sibling)
had the same mismatch and is fixed alongside `singletonMap`.

## Resolution (2026-07-18)

Both registration paths now allocate the real JDK wrapper whenever it is
available and initialize its natural fields by name:

- normal native-collections mode returns `Collections$SingletonSet` and
  `Collections$SingletonMap` with `element` / `k`,`v` populated;
- early/synthetic registration does the same for the complete singleton family
  (`List`, `Set`, and `Map`), retaining its old synthetic representation only
  as an unavailable-class fallback.

This restores the observable runtime type and the immutable contract instead
of exposing mutable `HashSet`/`HashMap` stand-ins. The conformance corpus now
checks all three wrapper classes and verifies that `singletonMap.put` throws.

Validation used the isolated Linux worktree
`/data/wt-class-getmethods-shadowing-closure-20260717-v2`, Java 25, and the
unique binary
`/data/cv-target-class-getmethods-shadowing-closure-20260717-v2/release/cratonvm-class-getmethods-shadowing-closure-20260717-v2`.
`SingletonProbe` printed `SINGLETONS_OK`: all three runtime names matched JDK
25 and `singletonMap` rejected mutation. The targeted Spring Boot sweep also
contained no `java.util.HashMap`/`SingletonMap` mismatch signature; this
fixture's `EnvironmentEndpointTests` could not be loaded because its cached
classpath contains Windows paths, an unrelated harness artifact.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-actuator` | `org.springframework.boot.actuate.env.EnvironmentEndpointTests` (1 of 17 failing test methods: `propertyWithComplexTypeShouldNotFail`) |
