# WebFlux `DefaultPathContainer` `DefaultSeparator` checkcast report

**Status: FIXED 2026-07-29**

## Original report

`org.springframework.boot.graphql.autoconfigure.reactive.GraphQlWebFluxAutoConfigurationTests`
had previously reported four reactive GraphQL failures wrapped as
`WebClientRequestException`, whose inner failure was a `ClassCastException`
from `java.lang.Object` to
`org.springframework.http.server.DefaultPathContainer$DefaultSeparator`.
The original analysis did not identify a CratonVM source location and
considered both stale/moved-GC references and duplicate class identity as
possible mechanisms.

## Closure

No targeted code change is required on the current `dev` head. A fresh
task-specific release binary was built and the entire affected class is now
green in both execution modes.

The user-supplied `C:\craton\CratonVM\apps\spring-boot` fixture was first
preflighted but could not be used as validation evidence: its generated
classpath lacked `Configurations.class`, and its incomplete source tree lacked
`build-plugin\spring-boot-antlib`. HotSpot consequently failed discovery with
`NoClassDefFoundError`, before any VM execution. Validation therefore used the
complete equivalent fixture at
`C:\craton\CratonVM-spring-boot-residual-20260728\apps\spring-boot`; its
HotSpot baseline passed all 17 tests.

## Validation

- HotSpot JDK 25 baseline: 17 tests, 0 failed, 0 aborted, 0 failed containers.
- CratonVM release, JIT on: 17 tests, 0 failed, 0 aborted, 0 failed containers
  (`SBRUNNER_RESULT tests=17 failed=0 aborted=0 skipped=0 containersFailed=0`).
- CratonVM release, `--nojit`: 17 tests, 0 failed, 0 aborted, 0 failed
  containers (`SBRUNNER_RESULT tests=17 failed=0 aborted=0 skipped=0
  containersFailed=0`).

The prior report is retired rather than attributing a cause to an
unreproduced failure.
