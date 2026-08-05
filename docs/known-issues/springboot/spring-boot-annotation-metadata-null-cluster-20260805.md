# Annotation/merged-annotation reflection metadata returns null under complex condition/property evaluation — 2 classes, 2 distinct null sites

**Status: OPEN — found 2026-08-05**

## Symptom

Both failures happen while Spring evaluates `@Conditional*`/`@ConfigurationProperties`
metadata for autoconfiguration classes, and both terminate context startup
with an unexpected null from annotation-reflection machinery. HotSpot passes
both classes cleanly on the same fixture (`hsfull-after-20260804-s3`), so
these are CratonVM-specific.

### `DataCassandraReactiveRepositoriesAutoConfigurationTests`

```
UnsatisfiedDependencyException: Error creating bean with name 'cassandraMappingContext' ...
  Unsatisfied dependency ... parameter 1: Error creating bean with name 'cassandraCustomConversions' ...
  Factory method 'cassandraCustomConversions' threw exception with message:
  Cannot invoke "org.springframework.core.annotation.MergedAnnotations.get(java.lang.Class)"
  because the return value of "org.springframework.core.annotation.MergedAnnotations.from(
  java.lang.reflect.AnnotatedElement, org.springframework.core.annotation.MergedAnnotations$SearchStrategy,
  org.springframework.core.annotation.RepeatableContainers)" is null
```
`MergedAnnotations.from(...)` — a static factory method that per its own
contract can never return null — returned null.

### `DataNeo4jReactiveRepositoriesAutoConfigurationTests`

```
java.lang.IllegalStateException: No ConfigurationProperties annotation found on
  'org.springframework.boot.data.neo4j.autoconfigure.DataNeo4jProperties'.
```
Fires 4 times (once per `@Nested`/test method) before the assertion failure.
`DataNeo4jProperties` genuinely carries `@ConfigurationProperties("spring.data.neo4j")`
in the fixture sources — Spring's own annotation-metadata lookup for a
present, valid annotation reports it absent.

## Root cause

Not identified at the source level in this pass. Both symptoms are the
reflective-annotation-metadata layer (`MergedAnnotations`,
`AnnotatedTypeMetadata`) disagreeing with the actual, present annotations on
a class — the same general shape (annotation lookup returns null/absent for
something that is provably there) as the already-tracked
`spring-bean-attribute-type-null-flake-20260803.md` (REGRESSED same day,
see that doc), but a different null site in each case: `MergedAnnotations.from()`
itself here, vs. an `IdentityHashMap` primitive-wrapper lookup there. Given
three annotation-metadata nulls surfacing across four classes on the same
day (this doc's two, plus couchbase's `attributeType` regression, plus
cache's later native-registry NoSuchMethodError), this looks like a systemic
weak spot in CratonVM's `MergedAnnotations`/ASM annotation-scanning path
under GC or class-loading pressure rather than 4 unrelated bugs, but no
single shared code-level defect was confirmed in the time available.

**Update 2026-08-05 — the cache member of that group was NOT a GC or
class-loading-pressure defect, so the grouping argument should be re-tested
before it is leant on.** It was a JIT native-dispatch defect: the per-call-site
native cache had been widened to serve non-leaf natives, so compiled code
dispatched targets `invoke_or_native` would never have reached and calls
returned their own first argument. The GC reading was refuted directly — the
crashing arm ran **zero** young collections. Fixed and retired to
`docs/internal/fixed-suite-bugs/springboot/cacheautoconfigurationtests-configclass-parse-nosuchmethod-FIXED.md`.
The two null sites here may share that cause; the cheap check is
`--dump-native-registry` on a JIT arm and a `--nojit` arm, diffed per native.
That named the cache defect in one comparison after code reading had stalled.

## Where to look next

`org/springframework/core/annotation/MergedAnnotations.java` and
`TypeMappedAnnotation`'s ASM-backed annotation scanner
(`org/springframework/core/type/classreading/`), and CratonVM's
`native-builtins` registrations backing `Class.getDeclaredAnnotations()` /
`AnnotatedElement` reflection (`grep -rn "getDeclaredAnnotations\|MergedAnnotations" native-builtins/ vm/ --include=*.rs`).
Both failures happen deep in the reactive-repositories condition/property
evaluation path, which is heavier on generic-type and annotation
introspection than most autoconfiguration classes — consistent with the
"structurally similar Data*ReactiveRepositoriesAutoConfigurationTests"
pattern the task noted.

## Affected classes

- `module/spring-boot-data-cassandra` — `org.springframework.boot.data.cassandra.autoconfigure.DataCassandraReactiveRepositoriesAutoConfigurationTests`
- `module/spring-boot-data-neo4j` — `org.springframework.boot.data.neo4j.autoconfigure.DataNeo4jReactiveRepositoriesAutoConfigurationTests`

## Related

- `docs/known-issues/springboot/spring-bean-attribute-type-null-flake-20260803.md` — same-day regression, same general "annotation/reflection metadata returns null" shape, different null site (data-couchbase).
