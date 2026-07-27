# `ConcurrentReferenceHashMap` computeIfAbsent repro

Standalone probe mirroring `RepeatableContainers$StandardRepeatableContainers`'s
`Map<Class<?>, Object>` cache pattern (real Spring `ConcurrentReferenceHashMap`,
`computeIfAbsent` caching either a real object or a `NONE` sentinel, with an
explicit cast + `!=` sentinel check on read — the exact shape that threw
`ClassCastException` in the (since-fixed) `RepeatableContainers` method-cache bug).

Needs only `spring-core-7.0.7.jar` on the classpath (no Spring Boot checkout,
no Gradle build).

```
javac -cp spring-core-7.0.7.jar CrhmRepro.java
java  -cp .;spring-core-7.0.7.jar CrhmRepro [numKeys] [rounds] [threads]
cratonvm --java-home <jdk25> -c .;spring-core-7.0.7.jar CrhmRepro [numKeys] [rounds] [threads]
```

Prints `MISMATCH`/`CCE` lines and a final `REPRO: BUG REPRODUCED` /
`REPRO: no bug observed` verdict, plus `ops`/`mismatches`/`cces` counts.
Exits 1 if any mismatch or `ClassCastException` was observed.

Note: the key-generation loop (many distinct dynamically-defined `Class`
objects) was previously ALSO subject to an unrelated JIT/OSR loop-duplication
bug (silent corruption from duplicate loop execution under OSR).
That bug is now FIXED on `dev`; `keys.size()` should equal `numKeys` again
regardless of JIT/OSR state. If it does not on the tree you're using, you are
likely on a pre-fix checkout.
