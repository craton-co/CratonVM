# `BatchDataMongoAutoConfigurationTests`: NoClassDefFoundError for a present classpath class (`MongoCustomConversions`)

**Status: OPEN — found 2026-08-05**

## Symptom

```
Caused by: java.lang.NoClassDefFoundError: org/springframework/data/mongodb/core/convert/MongoCustomConversions
    => java.lang.IllegalStateException: Unstarted application context [...][startupFailure=BeanCreationException] failed to start
     Caused by: org.springframework.beans.factory.BeanCreationException: Error creating bean with name
       'org.springframework.boot.batch.mongodb.autoconfigure.BatchDataMongoAutoConfiguration$SpringBootBatchMongoConfiguration':
       Failed to instantiate [...]: Constructor threw exception
     Caused by: java.lang.NoClassDefFoundError: org/springframework/data/mongodb/core/convert/MongoCustomConversions
```

`SpringBootBatchMongoConfiguration`'s constructor references
`MongoCustomConversions` (a `spring-data-mongodb` class), and the JVM reports
it as not-found — not a `ClassNotFoundException` from a classloader lookup,
but a `NoClassDefFoundError`, which typically means the class *was* resolved
once (e.g. at verification/linkage time) but failed to load/initialize
afterward, or an earlier failed load attempt was cached as permanently
absent.

## Not CratonVM-specific dependency absence

HotSpot passes this exact class on the same fixture
(`hsfull-after-20260804-s3`, `tests=13 failed=0`), so
`spring-data-mongodb`'s jar genuinely is on the runtime classpath for this
test — this rules out a simple missing-dependency/classpath-composition
issue and points at a CratonVM classloading defect specific to this class
(or a transitive class it references failing to link, surfacing as
`NoClassDefFoundError` for the outer class instead of the real cause).

## Root cause

Not identified in the time available. No existing doc matched
`MongoCustomConversions` or this exact `NoClassDefFoundError` shape by
keyword search across `docs/known-issues/` and
`docs/internal/fixed-suite-bugs/`.

## Where to look next

Re-run this single class with `CRATONVM_DBG=classload` (or equivalent) to
see whether `MongoCustomConversions` (or one of its supertypes/field types,
e.g. `org.springframework.core.convert.converter.Converter` generics-heavy
usage) hit a linkage error earlier in the run that got cached. Check whether
`MongoCustomConversions` or a class it depends on was already touched
(and failed) by an earlier test class in the same JVM process/module run —
a `NoClassDefFoundError` for a class that loads fine standalone is a classic
"first attempt threw, class marked erroneous, every later reference reuses
that cached failure" JVM behavior, so isolating whether this class passes
when run alone (vs. as part of the full module suite) would narrow this
quickly.

## Affected classes

- `module/spring-boot-batch-data-mongodb` — `org.springframework.boot.batch.mongodb.autoconfigure.BatchDataMongoAutoConfigurationTests`
