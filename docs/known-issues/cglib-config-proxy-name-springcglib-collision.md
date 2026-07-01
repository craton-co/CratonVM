# Synthetic `@Configuration` proxy can't be named `$$SpringCGLIB$$0` (AOT reflection-hint gap)

**Status:** OPEN · **Mode:** real-JDK, JIT on · HotSpot passes
**Dev verified on:** `ecfe859f`

## Symptom

`org.springframework.context.annotation.AnnotationConfigApplicationContextTests.refreshForAotRegisterHintsForCglibProxy`
fails. It expects an AOT reflection hint on `<Config>$$SpringCGLIB$$0`:

```java
context.register(CglibConfiguration.class);
RuntimeHints runtimeHints = new RuntimeHints();
context.refreshForAotProcessing(runtimeHints);
TypeReference cglibType = TypeReference.of(CglibConfiguration.class.getName() + "$$SpringCGLIB$$0");
assertThat(RuntimeHintsPredicates.reflection().onType(cglibType).withMemberCategories(
        INVOKE_DECLARED_CONSTRUCTORS, INVOKE_DECLARED_METHODS, ACCESS_DECLARED_FIELDS))
    .accepts(runtimeHints);
```

The member categories CratonVM registers are already IDENTICAL to HotSpot. The
**only** difference is the proxy class NAME:

| | proxy type registered |
|---|---|
| HotSpot | `CglibConfiguration$$SpringCGLIB$$0` |
| CratonVM | `CglibConfiguration$$EnhancerByCGLIB$$0` |

The AOT hint is registered by real Spring off `enhancedClass.getName()`, so it
lands under whatever name CratonVM's synthetic enhancer chose.

## Root cause

CratonVM's synthetic `@Configuration` enhancer
(`native-builtins/src/cglib_enhancer.rs`, `build_enhancer_class`) names its proxy
`<Config>$$EnhancerByCGLIB$$<hex>`. That name was chosen **deliberately** to avoid
colliding with **real** Spring CGLIB, which names its runtime proxies
`<base>$$SpringCGLIB$$<n>`.

CratonVM uses BOTH mechanisms: the synthetic enhancer for `@Configuration`
`@Bean` interception, and real Spring CGLIB for AOP / scoped / generics proxies.

## Why renaming to `$$SpringCGLIB$$0` does NOT work (tried & reverted)

Renaming the synthetic proxy to `<Config>$$SpringCGLIB$$<n>` (with per-base caching
+ collision-avoided index so it is a deterministic `$$0`, exactly matching HotSpot)
DOES make this test pass and the hint an exact match — **but it caused 9
regressions**:

```
Could not generate CGLIB subclass of class <X>$$SpringCGLIB$$0:
Common causes of this problem include using a final class or a non-visible class
```

in tests where the config bean ALSO gets AOP/scoped/generics-proxied:
`beanMethodThroughAopProxy`,
`genericsBasedInjectionWith{Early,Late}GenericsMatchingOn{Jdk,Cglib}Proxy`, etc.

Mechanism of the collision: when a config bean needs an AOP/scoped proxy, real
Spring CGLIB does `getUserClass(proxy)` → `Config`, then generates
`Config$$SpringCGLIB$$0` for its own proxy. That now collides with the synthetic
proxy's name. Real CGLIB's collision detection (`SpringNamingPolicy`'s `names`
predicate) only knows names IT generated — not the synthetic `defineClass` — so it
re-emits `$$0`, and CratonVM's `defineClass` rejects the duplicate →
`CodeGenerationException`.

## Path to a real fix (out of scope for a rename)

Needs a single naming authority so the two enhancers never collide, e.g.:

- route all `@Configuration` enhancement through real Spring CGLIB (drop the
  synthetic enhancer), or
- make real CGLIB's collision predicate see synthetic defines (so it advances to
  `$$1` when `$$0` is a synthetic class), or
- share one monotonic per-base index across both mechanisms.

Any of these is a larger architectural change. Until then this single AOT-hint
test stays failing; the `$$EnhancerByCGLIB$$` name must stay to keep real-CGLIB
interop working.

## Related (fixed) work in the same cluster

- `@Bean` fully-qualified bean-name resolution — FIXED (dev `73206779`).
- `checkLinkageError` (`printStackTrace(PrintWriter)` to a user stream) — FIXED
  (dev `2f7bad0d`).
