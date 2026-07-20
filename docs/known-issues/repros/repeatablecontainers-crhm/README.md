# `ConcurrentReferenceHashMap` computeIfAbsent repro

Standalone probe mirroring `RepeatableContainers$StandardRepeatableContainers`'s
`Map<Class<?>, Object>` cache pattern (real Spring `ConcurrentReferenceHashMap`,
`computeIfAbsent` caching either a real object or a `NONE` sentinel, with an
explicit cast + `!=` sentinel check on read — the exact shape that threw
`ClassCastException` in
`docs/internal/springboot/repeatablecontainers-method-cache-classcastexception-FIXED.md`).

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
objects) is itself subject to the unrelated JIT/OSR loop-duplication bug —
see `../jit-osr-loop-duplicate-execution/`. `keys.size()` printed by this
repro can come out LARGER than `numKeys` for large values on CratonVM with
JIT on; that is a symptom of the OTHER bug, not this one. Pass
`CRATONVM_JIT_OSR=0` (or keep `numKeys` under ~2000) if you need `keys.size()`
to reliably equal `numKeys` while investigating a `Map`-specific question.
