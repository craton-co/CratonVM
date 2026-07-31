# Rare: `@Bean` attribute resolution fails because a primitive return type is unmappable

| | |
|---|---|
| **Status** | **OPEN — observed once, not reproduced.** Narrowed to one line of Spring; two candidate mechanisms eliminated with checked-in probes. |
| **Category** | REFLECTION (`Method.getReturnType` / `Class.isPrimitive` on annotation attributes) |
| **Observed** | 2026-07-31, Azure `20.83.144.174`, `fix/spring-aot-cluster-residual-20260731` (`cratonvm-aotresid-r3.bin`), real JDK 25, host load ~50. One run out of seven of the same chunk. |
| **Impact** | Aborts AOT processing for the whole chunk — `rc=1`, no `PROBE RESULT`. Not a wrong answer, a hard failure. |

## What happened

During the closing sweep of [`../internal/fixed-suite-bugs/spring/spring-aot-cluster.md`](../internal/fixed-suite-bugs/spring/spring-aot-cluster.md),
chunk 9 aborted once while AOT-processing
`org.springframework.test.context.bean.override.mockito.integration.MockitoSpyBeanAndSpringAopProxyIntegrationTests`:

```
TestContextAotException: Failed to process test class [...] for AOT
 caused by IllegalArgumentException: Attribute 'autowireCandidate' for annotation
   [org.springframework.context.annotation.Bean] was not resolvable due to exception
 caused by NullPointerException: Cannot invoke "java.lang.Class.isArray()" because "attributeType" is null
   at TypeMappedAnnotation.adaptForAttribute(TypeMappedAnnotation.java:501)
```

Full log: `/data/tmp/allchunks-cv-r3/chunk.009.log`.

## Where the null comes from — exactly

`TypeMappedAnnotation.adaptForAttribute` line 500:

```java
Class<?> attributeType = ClassUtils.resolvePrimitiveIfNecessary(attribute.getReturnType());
if (attributeType.isArray() && ...)          // <- NPE here
```

and `ClassUtils.resolvePrimitiveIfNecessary`:

```java
Assert.notNull(clazz, "Class must not be null");
return (clazz.isPrimitive() && clazz != void.class ? primitiveWrapperTypeMap.get(clazz) : clazz);
```

`Assert.notNull` fires first, so `attribute.getReturnType()` did **not** return
null. The only other way out is null: `clazz.isPrimitive()` answered **true**
and `primitiveWrapperTypeMap.get(clazz)` — an `IdentityHashMap` keyed on the
`X.class` literals resolved inside `ClassUtils` — **missed**.

`Bean.autowireCandidate()` returns `boolean`. So one of these was true for that
one run:

* `getReturnType()` handed back a `boolean` mirror that is not the `boolean.class`
  `ClassUtils` holds; or
* `getReturnType()` handed back some *other* class whose `isPrimitive()` wrongly
  answered true, so the map was queried with a key that was never in it.

The second is the one this narrowing adds and is worth taking seriously — the
error message is the same either way, and `isPrimitive()` is the cheaper thing
to get wrong.

## Not reproduced, and what was ruled out

Do not re-run these two — they are checked in as `repros/primitive-mirror-identity/`
and both PASS on CratonVM, matching HotSpot:

* **`PrimitiveMirrorIdentityProbe`** — 20,000 rounds resolving all nine
  primitives through `X.class`, `X.TYPE`, `Method.getReturnType()`,
  `Class.getComponentType()`, and an annotation's own
  `annotationType().getDeclaredMethods()`, each checked for object identity
  *and* for presence in an `IdentityHashMap` keyed on the literals, with a
  `System.gc()` every 1024 rounds. **PASS.** A primitive mirror is a singleton
  under a single loader.
* **`CrossLoaderPrimitiveProbe`** — 300 parent-last child loaders, each with its
  own copy of the same class, comparing every primitive literal and
  `getReturnType()` result across the loader boundary and running Spring's exact
  map predicate. **PASS.** Primitive mirrors are not per-loader.

Also negative: 3 re-runs of chunk 9 on the same binary, 3 on the pre-fix binary,
and 8 runs of the failing class alone — all clean, no occurrence.

## Where to look next

The elimination above says the mirrors are fine *when nothing else is going on*.
What the failing run had that the probes do not: Mockito's inline mock maker had
redefined classes in-process, two loaders were live, and the host was at load
~50. Suggested order:

1. Instrument the real run rather than writing a third synthetic probe — the
   note at the end of the AOT-cluster doc applies verbatim. Compile a copy of
   `ClassUtils` that logs `clazz`, `System.identityHashCode(clazz)`,
   `clazz.isPrimitive()` and `clazz.getClassLoader()` on the null path, and put
   it ahead of the suite on `-cp`.
2. That single log line separates the two candidates outright: a wrong
   `isPrimitive()` shows a non-primitive name, a mirror-identity problem shows
   `boolean` with an unexpected identity hash.
3. Only then go looking in the VM.

Reproducing the sweep that found it:

```bash
cd /data/data/aot20260726
CRATONVM_BIN=<binary> ./allchunks.sh <tag>       # ~2 h, 20 chunks
```
