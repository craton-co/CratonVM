# Hibernate full-suite — wrong-result assertion FAILs (varied, CV-only)

> **Status (2026-06-19):** 🟡 PARTIALLY RESOLVED. The two crispest members
> (`UniqueConstraintBatchingTest`, `DetachedBagDelayedOperationTest`) shared a single root cause — native
> short-circuits of the **jboss-logging** facade that defeated the test infra's log interception. Fixed on
> branch `fix/hib-jboss-logger-base-intercept` (see **Resolved** below). The remaining members still need
> per-test investigation.
>
> A cluster of Hibernate ORM 8.0 tests that **fail an assertion with a wrong runtime result** (not a
> crash/hang/exception) on CratonVM but **PASS on HotSpot (JDK 25)**. Most members are likely **separate**
> root causes — they are grouped only because they share the "silently wrong value" shape, which is the
> hardest class to triage (no stack pointing at a native). Found in the dev full-suite census
> (`out-cratonvm-rerun`, latest dev).

These are distinct from the localized native bugs fixed this run (JSON SIGSEGV, JTA `getInetAddress`,
throwable order, `Locale.toLanguageTag`) and from the documented clusters (JTA XA, JAXB, deserialization,
Weld, Mockito MockMaker). They need **per-test** investigation: reproduce, diff the wrong value vs HotSpot,
and bisect to the diverging native (collection/JDBC/reflection/serialization).

## Members

| Class | Assertion | Likely area |
|---|---|---|
| ✅ `annotations.uniqueconstraint.UniqueConstraintBatchingTest` | `expected: <1> but was: <0>` | **FIXED** — not JDBC at all; the watched `SQLExceptionLogging.ERROR_LOG.warn(...)` event was swallowed by a native jboss-logging short-circuit (see **Resolved**) |
| ✅ `collection.delayedOperation.DetachedBagDelayedOperationTest` | `expected: <true> but was: <false>` | **FIXED** — not collection bookkeeping; the `HHH90030006` rollback watcher never fired because `DelegatingBasicLogger.isDebugEnabled()` was hard-stubbed to `false` + the base-`Logger` emit short-circuit (see **Resolved**) |
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

## Resolved (2026-06-19) — jboss-logging facade short-circuits defeated the test's log interception

**Branch:** `fix/hib-jboss-logger-base-intercept`. **Continues [HIB-CV-11](../../apps/hibernate-orm/cratonvm-bug-reports/HIB-CV-11-jboss-logging-testable-provider.md)**, which made `Logger.getLogger(...)` correctly return `hibernate-testing`'s `DelegatingLogger` but left "1 residual" (UniqueConstraintBatchingTest) — this is that residual, plus the bag test.

Both tests assert on **emitted log messages** via the testing infra (`@LoggingInspections`/`MessageKeyWatcher`, `LoggerInspectionExtension`/`Triggerable`). That infra works by installing a `DelegatingLogger` whose **`doLog`/`doLogf` override** records each event to registered `LogListener`s. Two native short-circuits in CratonVM bypassed that override, so **every watcher saw zero events** — the assertions then read the wrong value with no native frame to point at. Neither test's "likely area" (JDBC batch, collection bookkeeping) was the actual cause.

1. **`logmanager.rs` "Round 92"** registered natives on the abstract `org/jboss/logging/Logger` for `info/warn/error/fatal/debug/trace` (+ `f`/`v`/fqcn variants), emitting straight to stderr. That short-circuited virtual dispatch to the concrete subtype's `doLog`/`doLogf`, so `DelegatingLogger.doLog` never ran → no interception. This drove `UniqueConstraintBatchingTest` (`SQLExceptionLogging.ERROR_LOG.warn(...)` event swallowed) and the `testMergeDetachedCollectionWithQueuedOperations` watchers.
   **Fix:** gated the Round-92 block behind opt-in `CRATONVM_JBOSS_LOGGER_BASE_EMIT=1` (default OFF → real bytecode dispatches to the real `doLog`). WildFly boot visibility is preserved by the pre-existing **Round-90** `doLog`/`doLogf` intercepts on the concrete backend subclasses (`JBossLogManagerLogger`, …).

2. **`jboss_msc.rs` "R63"** registered `DelegatingBasicLogger.isTraceEnabled/isDebugEnabled/isInfoEnabled` to **always return `false`** (to avoid NPEs when WildFly's synthetic backfills leave `this.log` null). But `AbstractPersistentCollection.logDiscardedQueuedOperations()` only logs `HHH90030006` inside `if (COLLECTION_LOGGER.isDebugEnabled())` — so on rollback the message (and its watcher) was skipped, failing `testCollectionWithQueuedOperationsOnRollback`. The test's `log4j2.properties` sets `org.hibernate.orm.collection=debug`, so the real answer is `true`.
   **Fix:** these natives now **delegate to `this.log.isXEnabled()` when `this.log` is non-null**, returning `false` only when it is null (preserving the WildFly null-safety).

**Verified** vs HotSpot (binary `cratonvm-hiblog.exe`, JIT-on and `--nojit`): UniqueConstraintBatchingTest `ok=1`, DetachedBagDelayedOperationTest `ok=2`. No regression in the HIB-CV-11 log-inspection tests (`BootLoggingTests`, `SessionFactoryNamingTests` 5/5, `AnyTypeFlushToLoggableStringTest`, `ImmutableEntityUpdateQueryHandlingModeWarningTest`). Opt-out `CRATONVM_JBOSS_LOGGER_BASE_EMIT=1` restores the old short-circuit. Other full-suite log-assertion tests (e.g. `@MessageKeyInspection`/`Triggerable` users) likely benefit too.

**Still open (separate root causes):** `EntityGraphBatchSizeTest`, `ImmutableWithAttributeConverterTest` (serialization), `DriverManagerConnectionProviderValidationConfigTest`, `LoaderWithInvalidQueryTest` (asserts `getSuppressed().length==2`, not a log watcher), `DetachedPreviousRowStateTest`; `ClassLoaderServiceImplTest` = HIB-CV-16.
