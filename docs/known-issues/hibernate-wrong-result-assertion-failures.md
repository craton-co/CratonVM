# Hibernate full-suite — wrong-result assertion FAILs (varied, CV-only)

> **Status (2026-06-18):** 🔴 OPEN — handoff. A cluster of Hibernate ORM 8.0 tests that **fail an
> assertion with a wrong runtime result** (not a crash/hang/exception) on CratonVM but **PASS on HotSpot
> (JDK 25)**. Each is most likely a **separate** root cause — these are grouped only because they share the
> "silently wrong value" shape, which is the hardest class to triage (no stack pointing at a native).
> Found in the dev full-suite census (`out-cratonvm-rerun`, latest dev).

These are distinct from the localized native bugs fixed this run (JSON SIGSEGV, JTA `getInetAddress`,
throwable order, `Locale.toLanguageTag`) and from the documented clusters (JTA XA, JAXB, deserialization,
Weld, Mockito MockMaker). They need **per-test** investigation: reproduce, diff the wrong value vs HotSpot,
and bisect to the diverging native (collection/JDBC/reflection/serialization).

## Members

| Class | Assertion | Likely area |
|---|---|---|
| `annotations.uniqueconstraint.UniqueConstraintBatchingTest` | `expected: <1> but was: <0>` | JDBC batch insert / unique-constraint DDL count — a row/constraint not persisted or not counted |
| `collection.delayedOperation.DetachedBagDelayedOperationTest` | `expected: <true> but was: <false>` | detached `PersistentBag` queued (delayed) add/remove on merge — collection-event/dirty bookkeeping |
| `entitygraph.EntityGraphBatchSizeTest` | `AssertionError` (empty) | `@BatchSize` fetch under an entity graph — batch-fetch count/ordering |
| `bootstrap.registry.classloading.ClassLoaderServiceImplTest` | `expected:<1> but was:<0>` | **= HIB-CV-16** (user `ClassLoader` subclasses not virtualized) — already tracked |
| `annotations.immutable.ImmutableWithAttributeConverterTest` | `SerializationException: could not deserialize` | `@Immutable` + `AttributeConverter` round-trip; a converted/serialized column value not restoring |
| `connection.DriverManagerConnectionProviderValidationConfigTest` | `AssertionError` (empty) | connection-provider validation config — assertion on pool/validation state |
| `annotations.loader.LoaderWithInvalidQueryTest` | `AssertionFailedError` (empty) | expects a specific failure for an invalid `@Loader` query; CV diverges (no error or wrong error) |
| `jpa.DetachedPreviousRowStateTest` | `'…product' is detached` | detached-entity state across a row update — cascade/merge state |

(Membership grows as the census completes; `DriverManagerRegistrationTest`'s "Unanticipated failure
according to HHH-7272" and `ClassLoaderLeaksUtilityTest`'s `ClassNotFoundException` are likely
environmental/harness, **not** in this cluster.)

## Why grouped / why hard

A wrong-result assertion gives no native frame — the failure is observational ("HotSpot computes 1, CV
computes 0"). Triage requires running the single test, capturing the wrong value, and bisecting the data
path. Several plausibly trace to **native collection semantics** (batch lists, `PersistentBag` delayed ops,
batch-fetch) or **converter/serialization** round-trips, but none is yet pinned. Recommended order: the
two crispest (`UniqueConstraintBatchingTest` 1-vs-0, `DetachedBagDelayedOperationTest` true-vs-false), since
a boolean/count divergence is the easiest to bisect.

## Repro

Each via the `.cratonvm-suite` harness, one class at a time, e.g.:
`CRATONVM_DISABLE_JIT=1 cratonvm --java-home <jdk> @common.args -Dcraton.trace=1 -Dcraton.batch=1 CratonRunner <one-class-list> 0`
(`-Dcraton.trace=1` now prints a correctly-ordered stack for any *thrown* failure — the
`SerializationException` one — thanks to the throwable-order fix; pure `assertEquals` mismatches print only
the expected/actual values.)
