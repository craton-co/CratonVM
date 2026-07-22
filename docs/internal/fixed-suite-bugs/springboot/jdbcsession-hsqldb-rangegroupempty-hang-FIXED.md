# `JdbcSessionAutoConfigurationTests` HSQLDB `RangeGroupEmpty` loop — FIXED

**Status: FIXED 2026-07-15.** The original 300-second run was trapped in a
repeating out-of-bounds read of HSQLDB's zero-field
`RangeGroup$RangeGroupEmpty` singleton. The focused Spring Boot class now
completes successfully with JIT enabled: **PASS, 838.6 seconds** on the
dedicated optimized verification binary. The workload is slow but it is no
longer an infinite loop. The suite runner grants this exact class a 900-second
budget while retaining its normal 300-second default for other classes.

## Root cause

The invalid field access was not an HSQLDB layout or allocation problem.
`java.lang.reflect.Array.set(Object, int, Object)` handled every non-null
value for a primitive array as though it were a boxed primitive and read its
field slot 0. HSQLDB supplied a `RangeGroupEmpty` object in a path that must
raise `IllegalArgumentException`; the native unboxer instead read slot 0 of
the zero-field singleton. The benign heap guard returned null, allowing the
Java path to retry indefinitely.

`native-builtins/src/lib.rs` now validates both parts of the reflective-array
contract before touching heap data:

- the target is an actual array;
- a primitive-array value is a known boxed primitive (and is non-null).

Any other receiver/value now raises `IllegalArgumentException`, matching the
Java reflection API rather than treating arbitrary object layouts as wrappers.
The guard trace no longer reports `RangeGroupEmpty` after the change.

## Suite-runner residuals fixed

The reproduction also exposed two Windows runner defects, both corrected in
`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1`:

- per-class log stems are sized from the actual worktree/run directory, so a
  unique worktree cannot exceed the legacy `WriteAllText` path limit;
- an explicit `-CratonArgs --stack-dump-on-timeout=...` now overrides the
  runner's default disabled watchdog, allowing captured stack dumps without a
  duplicate CLI argument.
- this database-backed class receives the validated 900-second budget, so its
  ordinary slow execution is not reclassified as the retired hang.

## Validation

1. `cargo check -p cratonvm-native-builtins` passed.
2. `git diff --check` passed.
3. Built unique optimized binary
   `cratonvm-jdbcsession-hsqldb-20260715-003.exe` from this worktree.
4. Reproduced the old 300-second loop with
   `CRATONVM_DBG_OOBFIELD=RangeGroupEmpty`; its backtrace identified
   `native_array_set` unboxing the value argument.
5. Re-ran the exact class with JIT enabled and a 900-second per-class budget:
   `JdbcSessionAutoConfigurationTests` **PASS** (838.6 s), with no
   `RangeGroupEmpty` OOB diagnostics.
6. Re-ran through the normal suite invocation with `-TimeoutSec 300`; the
   targeted 900-second exception was applied and the class **PASS**ed in
   724.4 s.
