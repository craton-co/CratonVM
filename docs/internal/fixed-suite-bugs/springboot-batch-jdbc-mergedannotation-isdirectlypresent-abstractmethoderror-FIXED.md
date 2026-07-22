# `MergedAnnotation.isDirectlyPresent()` AbstractMethodError in Batch JDBC -- FIXED

**Status: FIXED -- verified 2026-07-17 on current `dev`**

## Original symptom

The 2026-07-17 Spring Boot rerun reported two Batch JDBC methods,
`testUsingJpa` and `testRegisteredAndLocalJob`, failing while importing
`HibernateJpaAutoConfiguration` with:

```
java.lang.AbstractMethodError: method
org/springframework/core/annotation/MergedAnnotation.isDirectlyPresent()Z
has no Code attribute
```

The concurrent `dataSource` destroy-method failures in the same class were
explicitly separate: they are the existing `Class.getMethods()` duplicate
method cluster, tracked in
[`jooq-destroy-method-ambiguity-and-hang-FIXED.md`](springboot/jooq-destroy-method-ambiguity-and-hang-FIXED.md).

## Root cause and resolution

The prior report correctly narrowed the failure to a stale receiver/class-tag
possibility rather than a missing Spring implementation. Commit `61eb24f94`
(`GCBARRIER-CDLWAIT-FIX`, now an ancestor of `dev`) forwards the receiver of
`getfield` and `putfield` after cold field resolution. That resolution can
load/link classes and allocate; before the fix, a moving collection could
leave the Rust-local receiver pointing at a structurally valid from-space
object. A subsequent virtual/interface invocation could consequently resolve
against the wrong class and fall through to an abstract interface declaration
with no Code attribute.

This is the exact mechanism hypothesized by the original report for a real
Spring annotation-metadata receiver. It is a general GC-root/forwarding fix,
not a Spring-specific native or a fabricated `MergedAnnotation` method.

## Verification

An isolated Azure worktree and private Spring Boot fixture copy were built
from current `dev`; the unique release binary was:

```
/data/data/cratonvm-bins/cratonvm-sb-batch-jdbc-mergedannotation-20260717-baseline
```

The fixture was rebuilt with JDK 25 to regenerate Linux classpaths, then
`org.springframework.boot.batch.jdbc.autoconfigure.BatchJdbcAutoConfigurationTests`
was executed directly through `SbRunner` in both modes:

| Mode | Result relevant to this issue |
|---|---|
| JIT | 34 tests started; zero `MergedAnnotation`, `AbstractMethodError`, or `CRATONVM_DBG_NOCODE` occurrences. Both formerly affected methods advanced into the separate destroy-method failure path. |
| `--nojit` | 34 tests started; 26 failures, all the same already-tracked `dataSource.shutdown` destroy-method ambiguity; zero target AbstractMethodError occurrences. |

HotSpot runs the same current class 34/34. CratonVM's remaining 26 failures
are not residuals of this issue and remain in the open destroy-method tracker.

## Historical evidence

The original log was:

`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/module_spring-boot-batch-jdbc.org.springframework.boot.batch.jdbc.autoconfigure.BatchJdbcAutoC-adef578bf5b8.out.log`

It was captured from the older full-suite worktree at `213d93ea`; current
`dev` contains `61eb24f94` and the clean focused reruns above. This document
is retained as the closure record; active known-issue documentation contains
only the unrelated destroy-method residual.
