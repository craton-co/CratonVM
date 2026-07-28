# Spring Boot logging cross-method state leakage pattern retired

**Status: FIXED/RETIRED — validated 2026-07-28**

## Original observation

The 2026-07-23 residual report grouped several failures that looked like
test-method state leaking through one `SbRunner` JVM: a stale Log4j2 rolling
policy system property and stale Logback pattern-rule types in AOT hints. It
was explicitly a pattern-level hypothesis, not a confirmed common root cause.

## Closure evidence

The original supplied Spring Boot fixture no longer contained the compiled
`core/spring-boot` test classes or classpath file. A clean Spring Boot checkout
at `55fae927d7003fc3158751e854455d058d9cfda0` was therefore compiled with the
same JDK 25 runner setup. Each complete class was executed in one `SbRunner`
process with a fresh CratonVM process per class. Every started test completed
with zero failures, aborts, skips, and container failures:

| Class | JIT | `--nojit` |
|---|---:|---:|
| `DefaultLogbackConfigurationTests` | 7/7 | 7/7 |
| `Log4j2LoggingSystemPropertiesTests` | 3/3 | 3/3 |
| `LogbackConfigurationAotContributionTests` | 11/11 | 11/11 |

`probes/CrossMethodStateLeakageProbe.java` additionally covers the two shared
contracts implicated by the original report: `System.getProperties().keySet()`
must restore the real system-property view after `retainAll`, and
`LoggerContext.reset()` must clear an object registered by a preceding test.
It prints `CROSSMETHOD_PROBE_OK` in both JIT modes.

## Resolution

There is no remaining reproducible cross-method state leak in this scope and
no new common VM defect to fix. The earlier symptoms have been retired only
after complete class-level validation in both modes; the separate console and
uncategorized issue records remain independent of this retired pattern.
