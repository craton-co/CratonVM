# Rare: `@Bean` attribute resolution fails because a primitive return type is unmappable

| | |
|---|---|
| **Status** | **OPEN — observed once, still not reproduced** in ~130 instrumented runs. A large adjacent defect was found and fixed on the way (see "What was fixed"), and it may or may not be the cause. Two tools now exist that did not before: a VM-side miss audit and an instrumented `ClassUtils`. |
| **Category** | REFLECTION / COLLECTIONS (`Class.isPrimitive` + an `IdentityHashMap` lookup) |
| **Observed** | 2026-07-31, Azure `20.83.144.174`, `cratonvm-aotresid-r3.bin`, real JDK 25, host load ~50. One run out of seven of the same chunk. |
| **Impact** | Aborts AOT processing for the whole chunk — `rc=1`, no `PROBE RESULT`. Not a wrong answer, a hard failure. |

## What happened

During the closing sweep of
[`../../internal/fixed-suite-bugs/spring/spring-aot-cluster.md`](../../internal/fixed-suite-bugs/spring/spring-aot-cluster.md),
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
return (clazz.isPrimitive() && clazz != void.class ? primitiveTypeToWrapperMap.get(clazz) : clazz);
```

`Assert.notNull` fires first, so `attribute.getReturnType()` did **not** return
null. The only other way out is null: `clazz.isPrimitive()` answered **true**
and `primitiveTypeToWrapperMap.get(clazz)` — an `IdentityHashMap` keyed on the
`X.class` literals resolved inside `ClassUtils` — **missed**.

`Bean.autowireCandidate()` returns `boolean`. So either the mirror is not the
one `ClassUtils` holds, or `isPrimitive()` was wrong about a class that is not
primitive at all, or the lookup itself misbehaved once.

## The single most useful fact learned since

**It is one bad lookup, not a broken map.** An instrumented `ClassUtils`
(`/data/tmp/patch-classutils2.sh` rebuilds it) checks all nine primitives every
64th call and reports the first moment any of them is unmappable. Across
**~130 chunk-9 runs** — thousands of integrity checks each — it never fired,
while the original failure is a lookup that returned null. A map that were
transiently broken for any meaningful window would have been caught many times
over.

That also rules out the tidy explanations: the map is intact before and after,
so this is not a lost entry, not a resize race, and not a hash that changed and
stayed changed.

## Not reproduced, and what is ruled out

Do not re-run these — they are checked in as passing probes and are
*eliminations*, not reproducers:

* **`repros/primitive-mirror-identity/PrimitiveMirrorIdentityProbe`** — a
  primitive mirror is a singleton within a loader. 20,000 rounds across six
  reflective routes, identity **and** map-membership checked, `System.gc()`
  every 1024 rounds. PASS.
* **`repros/primitive-mirror-identity/CrossLoaderPrimitiveProbe`** — primitive
  mirrors are not per-loader. 300 parent-last loader pairs running Spring's
  exact map predicate across the boundary. PASS.
* **`IdentityHashStabilityProbe`** (`/data/tmp`) — `System.identityHashCode` is
  stable across GC for class mirrors, plain objects and reflected return types,
  and an `IdentityHashMap` keeps finding keys it holds. PASS.
* **`ResolvePrimitiveHotProbe`** (`/data/tmp`) — 20,000,000 calls to the real
  `ClassUtils.resolvePrimitiveIfNecessary` over 23 receivers, well past
  tier-up, every result checked. PASS. So the compiled form of the method is
  not obviously wrong.

Also negative: 3 re-runs of chunk 9 on the same binary, 3 on the pre-fix binary,
8 runs of the failing class alone, and ~130 runs under the two detectors below.

## What was fixed on the way

Chasing this found a **different, much larger defect** with the same trigger:
after any class redefinition, CratonVM's synthetic collections fell back to real
JDK bodies that index a field graph the objects do not have. `TreeMap.get`
returned null, `ConcurrentHashMap.size()` returned 0, `HashMap.keySet()` came
back empty — 32 of 89 operations, mostly silently. See
[`../repros/redefine-collection-layout/`](../repros/redefine-collection-layout/).

That is squarely in this bug's neighbourhood — Spring's `AnnotationAttributes`
*is* a `LinkedHashMap`, the failing path is annotation-attribute machinery, and
Mockito had redefined classes in that process — **but it is not a proven cause
of this NPE.** The failing lookup is an `IdentityHashMap`, and `IdentityHashMap`
was one of the collections the probe showed surviving. Re-check this doc after
some soak time on the fix rather than assuming it closed it.

## An adjacent sighting worth knowing about

While A/B-ing that fix, a *wrong* dispatch decision produced this, ~1 run in 8
in chunk 3:

```
NoSuchMethodError: java.lang.Integer.isArray()Z
NoSuchMethodError: java.lang.Integer.represents(Ljava/lang/reflect/Type;)Z
  at TypeDescription$Generic$Visitor$Substitutor.onNonGenericType
```

That was self-inflicted — an over-broad immunity change, since reverted, and it
does not occur on any shipped binary. It is recorded here because the **shape**
is the same as this bug: a `Class`-typed slot in ByteBuddy/reflection machinery
holding something that is not a `Class`. Here it was an `Integer`; in this bug it
is a `Class` that answers `isPrimitive()` but is not in the primitive map. If a
mechanism is ever found that puts the wrong object into one of those slots, it is
worth testing against both.

## Two detectors that now exist

1. **`CRATONVM_DBG_MAP_MISS_AUDIT=1`** — on any map miss, re-walk the buckets;
   if a node's key is the *same object* as the one searched for, print the
   searched hash and bucket against the stored hash and the bucket the key
   actually lives in. Quiet on legitimate misses, capped at 4096 buckets. This
   is the tool that turns "one bad lookup" into a diagnosis; it has not yet been
   run at the moment the bug fires.
2. **An instrumented `ClassUtils`** — `bash /data/tmp/patch-classutils2.sh`
   builds it into `/data/tmp/cuinstr`, which must go **first** on `-cp` to
   shadow `spring-core.jar`. It dumps every map entry with identity hashes,
   `==` against the argument, and a stack.

Hunt with both:

```bash
CRATONVM_BIN=<binary> /data/tmp/hunt-beanflake2.sh <tag> 60 3g
```

Run it 6-wide; each chunk-9 run is ~5 minutes. Note the build host OOM-kills a
`cargo build` if six workers are running — stop them before rebuilding.

## Where to look next

The remaining candidates, in the order they are worth testing:

1. `Class.isPrimitive()` answering true for something that is not primitive. It
   reads a **hard-coded slot 7** (`native_class_is_primitive`,
   `native-builtins/src/lang_class.rs`) while
   `get_or_create_primitive_mirror` writes the flag at a **name-resolved**
   slot. Those agreeing is an assumption, not a check. A mirror that acquires a
   non-zero Int at slot 7 by any other route reads as primitive.
2. The miss audit firing — that separates a hash mismatch from a bucket
   mismatch outright.
3. Only then, the lookup path itself.
