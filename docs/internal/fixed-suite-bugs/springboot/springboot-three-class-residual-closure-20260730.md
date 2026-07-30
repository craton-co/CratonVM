# Spring Boot three-class residual closure

**Status: FIXED — validated 2026-07-30.**

The supplied `apps/spring-boot` checkout was incomplete (`build-plugin/spring-boot-antlib`
was absent), so it could not generate test classpaths. Validation used the source-complete
equivalent fixture `C:\craton\CratonVM-spring-boot-log4j2-fixture-20260729-019faf10`.

## Resolution

- Isolated URL-loader method signatures now link their return type while reflective methods
  are materialized, translating a failed transitive linkage to `NoClassDefFoundError`. This
  lets `OnBeanCondition` retain its required `BeanTypeDeductionException` cause chain.
- Windows `Path` allocation removes a trailing separator from ordinary paths while retaining
  drive, UNC, and virtual-filesystem roots.
- JMX snapshots retain the public CountDownLatch identity but expose its logical
  `CountDownLatch$Sync` class in `ThreadInfo` parking output.

## Validation

The unique release binary `cratonvm-sbthree-complete-20260729-019fb092.exe` passed the
three-class list in both modes:

- JIT: 3/3 classes, 15/15 tests, zero failures, aborts, skips, and container failures.
- `--nojit`: 3/3 classes, 15/15 tests, zero failures, aborts, skips, and container failures.

Result TSVs:

- `C:\craton\sbthree-suite-20260729-019fb092\results\craton-jit-fixed7-20260729-019fb092\all-jit\results.tsv`
- `C:\craton\sbthree-suite-20260729-019fb092\results\craton-nojit-fixed7-20260729-019fb092\all-nojit\results.tsv`
