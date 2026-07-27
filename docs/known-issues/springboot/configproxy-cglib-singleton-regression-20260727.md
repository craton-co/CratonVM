# `S03_ConfigProxy` CGLIB `@Configuration` proxy singleton-identity regression (2026-07-27)

## Summary

The real Spring Boot functional suite at
`/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite`
(real `spring-boot-4.0.6.jar` + Spring Framework 7.0.7 jars) previously
documented **10/10 scenarios matching HotSpot** as of 2026-06-11
(`RESULTS.md`, run `run-20260611-221041`, following the real CGLIB
`@Configuration` `@Bean`-method interception fix in `cglib_enhancer.rs`).

Re-running the full suite today (2026-07-27, dev @ `b8225b18c`) found
`S03_ConfigProxy` has regressed to **4/8**, identically in every
configuration tested (default JIT baseline, default JIT with
`org/springframework/boot/` allow-listed, `CRATONVM_JIT_THRESHOLD=1`
aggressive with the same allow-list, and aggressive with the ban still
active) -- i.e. this is **not** a JIT-ban-related regression; something else
changed in the intervening six weeks of `dev` history.

## Repro

```
CP=/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite/classes:$(ls /data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite/lib/*.jar | tr '\n' ':')
/data/data/cratonvm/target/release/cratonvm --java-home /data/jdk25-real-20260717/jdk-25.0.3+9 -cp "$CP" S03_ConfigProxy
```

## Failure detail

```
[T] cglib.car1SharesEngine: FAIL
[T] cglib.car2SharesEngine: FAIL
[T] cglib.sameEngineAcrossCars: FAIL
[T] cglib.engineBuiltOnce: FAIL expected=[1] actual=[3]
SUITE S03_ConfigProxy passed=4 failed=4 total=8
```

`engineBuiltOnce` expects the `@Bean` factory method to run exactly once
(singleton semantics via the CGLIB `@Configuration` proxy's
`$$beanFactory` cache) but it ran 3 times -- the proxy is no longer
returning the cached instance across repeated in-context bean lookups
that reference each other (`car1SharesEngine`/`car2SharesEngine`/
`sameEngineAcrossCars` all check that two beans injected with the same
`@Bean`-returned dependency get object-identical instances).

## Scope note -- confirmed NOT a JIT issue

This is unrelated to any `org/springframework/boot/*` JIT ban -- it
reproduces byte-identically whether SPB.4/.4b/.4c are banned or lifted,
under both default and aggressive JIT thresholds, **and also reproduces
identically with `CRATONVM_DISABLE_JIT=1` (JIT fully off)**:

```
CRATONVM_DISABLE_JIT=1 cratonvm --java-home <jdk25> -cp "$CP" S03_ConfigProxy
# -> same 4/8, same four failing checks
```

So this is a pure interpreter/native-level regression, not a JIT
miscompile at all. It is most likely in `cglib_enhancer.rs`'s
`$$beanFactory` singleton-cache implementation itself, or in whatever
dispatches to it (proxy-class loading/identity, or a native override
introduced since 2026-06-11).

## Next steps for a follow-up session

- Bisect `dev` history for `cglib_enhancer.rs` and its call sites between
  the 2026-06-11 all-green run (dev `7c4ce692`) and now -- since JIT is
  ruled out, look for changes to CGLIB proxy generation, bean-factory
  caching, or classloading/identity in that window.
- The suite's own `results/` directory has the full historical run log
  for comparison (`results/run-20260611-221041`).
