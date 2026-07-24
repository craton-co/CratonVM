# Hibernate suite — AssertionFailedError long-tail catalog fixed

| | |
|---|---|
| **Status** | FIXED — catalog retired 2026-07-15. |
| **Original scope** | The 2026-07-11 scattered Hibernate assertion/timeout/schema catalog, including every residual retained at the 2026-07-14 checkpoint. |

The original triage note was intentionally a catalog rather than a single
failure mechanism. Its resolved clusters were rechecked on the final
real-JDK CratonVM binary, and its previously unclassified timeout,
schema-generation, and one-off entries were run to completion. No catalog
entry remains open.

## Runtime fixes

- Restored real-JDK `java.lang.instrument` and in-process Attach API
  registrations as VM bridges. The real-JDK registration path had dropped
  them as synthetic stubs, breaking Mockito's dynamic self-attach and the
  UUID generator tests.
- Kept the real `InstrumentationImpl` mirror as a VM-owned bare allocation;
  its JDK constructor requires JVMTI-native state which CratonVM does not
  provide.
- Extended the conservative, liftable JIT cold-path guard to ordinary
  `org/antlr/v4/runtime/`. Hibernate's unshaded parser could corrupt
  `ATNState.transitions` after a prior HQL parse, surfacing as
  `NullPointerException` in `ParserATNSimulator.computeTargetState`.
- The closure build also includes the earlier catalog fixes for H2/native
  execution, loader fidelity, ANTLR recovery, and dirty-tracked persistent
  collection sorting.

## Final validation

All runs used the uniquely built real-JDK binary with JIT enabled and a
single Gradle worker.

- UUID/self-attach: `UUidV6V7GeneratorTest` — 2/2 passed.
- Timeout residuals: the remaining 11 classes — 20 tests, 0 failures/errors;
  together with the already rechecked fetch-profile and UUID entries this
  closes all 13 historical timeout entries.
- Schema generation: the four `jpa.schemagen.*` classes — 19 tests, 0
  failures/errors.
- One-off residuals: 45 tests across the remaining array, JSON, mapping,
  filter, custom-SQL, lazy-to-one, and persistence-unit classes. The final
  rerun passed all except the deliberately isolated JSON parser issue below;
  its companion rerun then passed 5/5.
- `JsonArrayUnnestTest`: full class rerun after the ANTLR guard — 5 tests, 0
  failures/errors in 663.304 seconds. This includes the parse sequence that
  previously failed with null `ATNState.transitions`.

The long JSON/H2 lateral-unnest query remains expensive under CratonVM, but
completed successfully and returned the expected rows; it is not a failing
or timed-out catalog residual.

## Historical context

The original note's approximate class counts and preliminary root-cause
theories were snapshots of the 2026-07-11 audit. They are retained in Git
history only and must not be used as current known issues.
