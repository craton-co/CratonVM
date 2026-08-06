# RESOLVED 2026-08-06: `isPrimitive()` was answering with another call site's boolean

**Status: RESOLVED.** The 2026-08-05 regression recorded below is the
recycled-`JitInvokeInfo` dispatch defect fixed by `383e7f5cf`, which landed
*after* the full-suite run that sighting came from. Full evidence — the
aliased-pair census, the pre-fix/post-fix A/B, and the disassembly — is in
[`spring-boot-annotation-metadata-null-cluster-RESOLVED-20260806.md`](spring-boot-annotation-metadata-null-cluster-RESOLVED-20260806.md).
This page is kept for its tooling and its eliminations, both of which stay
valid.

**The one thing worth carrying forward.** This page's own "where to look next"
ranked *"`Class.isPrimitive()` answering true for something that is not
primitive"* **first**. That was correct. What it got wrong was the layer: it
looked at the mirror's `primitive` slot, and the defect was in the dispatch
that delivered the answer. `CRATONVM_DBG_SITE_ALIAS=1` on
`DataCouchbaseReactiveRepositoriesAutoConfigurationTests` prints, among ~1000
site keys, both of these:

```
key=... WAS org/springframework/core/annotation/AnnotationTypeMapping.getDistance()I
        NOW java/lang/Class.isPrimitive()Z
key=... WAS java/lang/Class.isAssignableFrom(Ljava/lang/Class;)Z
        NOW java/lang/Class.isPrimitive()Z
```

A freed `JitInvokeInfo` address re-issued to the `isPrimitive()` site let it
return `getDistance()`'s non-zero `int` as its boolean. So
`resolvePrimitiveIfNecessary` asked `primitiveTypeToWrapperMap` for a class
that is **not** primitive, and the map correctly missed — which is exactly the
"one bad lookup, not a broken map" this page could never explain, and exactly
why ~1030 instrumented hunt runs caught nothing: both detectors watched the
map, and the map was innocent throughout.

The two fixes below are still good on their own merits (the `IdentityHashMap`
one is a genuine spec violation, provable without ever seeing this bug), but
neither was the cause of the sighting.

The original 2026-07-31 sighting is *consistent* with the same cause — the
site-keyed memo family dates to `fc012ab1b`, 07-31 — but was not re-measured
here and is not claimed as proven.

Verified green on current `dev`: `DataCouchbaseReactiveRepositoriesAutoConfigurationTests`
0 failures in 20 runs (4 lanes concurrent), against 1 in 24 on a `1078f6f05c`
anchor built from the pre-fix tree.

---

# Rare: `@Bean` attribute resolution fails because a primitive return type is unmappable

*(page as it stood while open)*

## Regression note (2026-08-05)

Reproduced with the *identical* signature (`NullPointerException: Cannot
invoke "java.lang.Class.isArray()" because "attributeType" is null`, at
`TypeMappedAnnotation.adaptForAttribute`) in a completely different caller
than the original report: `module/spring-boot-data-couchbase`'s
`DataCouchbaseReactiveRepositoriesAutoConfigurationTests`, evaluating the
`@ConditionalOnProperty`'s `matchIfMissing` attribute (also a primitive
`boolean`) on `CouchbaseClientFactoryConfiguration` during condition
processing — not the original report's `@Bean.autowireCandidate()` path.

```
11:52:09 [main] WARN AnnotationConfigApplicationContext -- Exception encountered during context initialization -
  cancelling refresh attempt: org.springframework.beans.factory.BeanDefinitionStoreException: Failed to process
  import candidates for configuration class [DataCouchbaseAutoConfiguration]: Error processing condition on
  CouchbaseClientFactoryConfiguration
Caused by: java.lang.IllegalStateException: Error processing condition on CouchbaseClientFactoryConfiguration
Caused by: java.lang.IllegalArgumentException: Attribute 'matchIfMissing' for annotation
  [org.springframework.boot.autoconfigure.condition.ConditionalOnProperty] was not resolvable due to exception
  [java.lang.NullPointerException: Cannot invoke "java.lang.Class.isArray()" because "attributeType" is null]
Caused by: java.lang.NullPointerException: Cannot invoke "java.lang.Class.isArray()" because "attributeType" is null
```

HotSpot passes this class cleanly (`hsfull-after-20260804-s3`,
`tests=6 failed=0`), same fixture, so this is not a CRLF/classpath artifact.
Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-fullsuite-azure-20260805-s4/all-jit/logs/module_spring-boot-data-couchbase.org.springframework.boot.data.couchbase.autoconfigure.DataCouchbaseReact-da37a2c78c76.out.log`

The 2026-08-03 closure was explicit that it had never caught the bug live —
it closed on "two real defects found and fixed, both matching the report's
own analysis, plus negative evidence" (0 reproductions in ~1030 hunt
attempts), not on a confirmed kill. This 08-05 sighting is the first known
live reproduction since that closure, on a *different* attribute
(`matchIfMissing` vs `autowireCandidate`) and a *different* IdentityHashMap
consumer (`ConditionalOnProperty` condition evaluation vs `@Bean` attribute
merging) — same underlying `ClassUtils.resolvePrimitiveIfNecessary` /
`primitiveTypeToWrapperMap` (`IdentityHashMap<Class<?>, Class<?>>`) miss for
a primitive `Class` mirror, confirming this is a real, still-live gap and
not something specific to the original caller. Re-open per the doc's own
"re-open if it resurfaces" instruction.

Also worth checking: `module/spring-boot-data-cassandra`'s
`DataCassandraReactiveRepositoriesAutoConfigurationTests` failed the same day
with a related-looking but not identical NPE — `MergedAnnotations.get(Class)`
invoked on a null return from `MergedAnnotations.from(...)` — filed
separately (not folded into this doc, since the exact null site differs and
the chain doesn't obviously funnel through `resolvePrimitiveIfNecessary`).
See `spring-boot-annotation-metadata-null-cluster-20260805.md`.

---

## Closure (2026-08-03)

This bug was never caught live — the map-miss audit and instrumented
`ClassUtils` the original report built were never running at the moment it
fired, and it still hasn't fired again since (0/1030+ hunt attempts across
this and the prior session, both before and after the fixes below). So this
is closed on the strength of two real defects found and fixed, both matching
the original report's own analysis, plus an unusually large amount of
negative evidence — not on having watched the mechanism fire and confirmed
the fix stops it. Re-open if it resurfaces.

### Fix 1 — exactly the report's own "where to look next" candidate #1

`native_class_is_primitive` (`native-builtins/src/lang_class.rs`) read a
**hard-coded instance slot 7** for `java/lang/Class`'s `primitive` flag, on
the unchecked assumption that slot 7 is wherever
`get_or_create_primitive_mirror`'s **name-based** field resolution
(`resolve_class_mirror_slots` in `vm/src/vm/vm_object.rs`) happens to land it
for this JDK build. The report named this precisely: "Those agreeing is an
assumption, not a check." It is now resolved by field name, the same way the
writer resolves it, cached once found — so reader and writer agree by
construction instead of by coincidence, and a mirror that hasn't loaded
`java/lang/Class` in its final form yet gets rechecked instead of the
resolution being cached as permanently absent.

For `boolean.class` on the JDK build this was tested against, slot 7 already
happened to be correct — this closes a hazard for a build where it isn't,
not a confirmed hit on this exact failure. But it removes the "assumption,
not a check" the report flagged as unaudited.

### Fix 2 — a defect the report didn't name: `IdentityHashMap` wasn't using identity

The failing map, `ClassUtils.primitiveTypeToWrapperMap`, is a real
`java.util.IdentityHashMap<Class<?>, Class<?>>`. CratonVM materializes it as
a synthetic bucket map (same 3-field layout as `HashMap`), and its
`get`/`put`/`remove`/`containsKey` were registered directly onto the
**generic** bucket-map natives — the same ones `HashMap` uses. Those compute
the bucket hash and collision equality via the key's **virtual**
`hashCode()`/`equals()` — content semantics, not the reference identity
`IdentityHashMap`'s contract requires.

This happened to be invisible for `java.lang.Class` keys specifically,
because `Class` doesn't override either method (both fall back to
`Object`'s, which are identity-based anyway) — which is exactly why nothing
caught it before. But every `hashCode()`/`equals()` dispatch it triggered was
a real, unnecessary moving-GC window that a pure identity computation never
needs — and this codebase has fixed the same *shape* of bug (a stale
`ObjectRef` surviving a GC triggered mid-dispatch inside this exact bucket-map
code) multiple times before in unrelated contexts (see the `S111r27`,
`HIB-MAPPUT-PINORDER.1`, and WildFly-parallel-boot-stale-objectref comments
already in `native-collections/src/lib.rs`). A receiver classification
(`CF_IDENTITY_MAP`) now routes `IdentityHashMap` (and subclasses) straight to
`ctx.identity_hash_code()` + pointer equality, matching real
`IdentityHashMap` semantics and removing that dispatch surface for this map
family entirely — for `get`, `put`, `remove`, and `containsKey`, including
the string/int fast-path overlays that would otherwise collapse
equal-content-but-not-identical keys.

This is the stronger fix of the two: it's a genuine spec violation (provable
without ever seeing this bug fire — `new IdentityHashMap<>()` given two
`.equals()`-but-not-`==` keys behaved like a content map before this), and it
eliminates an entire class of GC-timing risk from every `IdentityHashMap`
operation in the VM, not just this one call site.

### Validation

* Full `native-collections` and `native-builtins` test suites (3240+ tests)
  pass clean, before and after merging in ~90 unrelated commits that landed
  on `dev` during this session.
* `hunt-beanflake2.sh` (chunk 9, both detectors — `CRATONVM_DBG_MAP_MISS_AUDIT`
  and the instrumented `ClassUtils`) run **900 times** against the fixed
  binary across three rounds, 6-wide: **0 reproductions, 0 detector hits**.
  Combined with the ~130 runs from the original investigation, that's
  **~1030 total attempts**, none of which caught the failure either before or
  after the fix.
* The "What was fixed on the way" defect below (redefine-collection-layout)
  was independently confirmed already merged into `dev` (`4a647c9071`,
  ancestor-checked) — soak time has passed with no recurrence.

### A separate, unrelated, more severe finding made along the way

While validating the fix against the latest `dev` (merged in ~90 commits
since this investigation started), `hunt-beanflake2.sh` against chunk 9
started failing **100% of the time** — not with this bug, but with a javac
**internal compiler `AssertionError`** (`Check$SuperThisChecker.check`,
`checkSuperInitCalls`) while CratonVM self-hosts javac 25.0.3 to compile
Spring's AOT-generated sources, immediately after CratonVM's JIT logs bailout
warnings on javac's own `ClassReader` methods. A vanilla `origin/dev` build
(no fixes from this doc applied) reproduces the identical failure, so this is
confirmed **pre-existing on `dev`, unrelated to either fix above** — not a
regression from this work. It blocks the Spring AOT test cluster entirely
and needs its own investigation; flagged separately rather than folded into
this closure.

---

# Rare: `@Bean` attribute resolution fails because a primitive return type is unmappable

*(original report, verbatim)*

| | |
|---|---|
| **Status** | **OPEN — observed once, still not reproduced** in ~130 instrumented runs. A large adjacent defect was found and fixed on the way (see "What was fixed"), and it may or may not be the cause. Two tools now exist that did not before: a VM-side miss audit and an instrumented `ClassUtils`. |
| **Category** | REFLECTION / COLLECTIONS (`Class.isPrimitive` + an `IdentityHashMap` lookup) |
| **Observed** | 2026-07-31, Azure `20.83.144.174`, `cratonvm-aotresid-r3.bin`, real JDK 25, host load ~50. One run out of seven of the same chunk. |
| **Impact** | Aborts AOT processing for the whole chunk — `rc=1`, no `PROBE RESULT`. Not a wrong answer, a hard failure. |

## What happened

During the closing sweep of
`fixed-suite-bugs/spring/spring-aot-cluster.md`,
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

## Affected classes

- `module/spring-boot-data-couchbase` — `org.springframework.boot.data.couchbase.autoconfigure.DataCouchbaseReactiveRepositoriesAutoConfigurationTests` (regression, 2026-08-05)
