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
| ✅ `annotations.immutable.ImmutableWithAttributeConverterTest` | `SerializationException: could not deserialize` | **FIXED** — `@AttributeConverter` stores a `Serializable` `Exif` (wraps a HashMap) as a serialized column; `new HashMap<>(src)` left the real `loadFactor`/`table` unset so `writeObject` emitted `loadFactor=0.0` + 0 entries (see **Resolved 3**) |
| ✅ `connection.DriverManagerConnectionProviderValidationConfigTest` | `AssertionError` (empty) | **FIXED** (now passes; a watched-log assertion — resolved by the jboss-logging fix) |
| ✅ `annotations.loader.LoaderWithInvalidQueryTest` | `AssertionFailedError` (empty) | **FIXED (nojit)** — not a `@Loader` issue; asserts `getSuppressed().length == 2`, but `Throwable.addSuppressed` silently dropped everything because shadowed `Throwable.<init>` left `suppressedExceptions` null (see **Resolved 2**). Still times out under **JIT** (separate pre-existing ANTLR-HQL-parse slowness, not the suppressed bug) |
| ✅ `jpa.DetachedPreviousRowStateTest` | `'…product' is detached` | **FIXED** — not cascade/merge; CratonVM's synthetic `Stream` was eager, so `getResultStream().forEach(...)` buffered all rows before the per-row `flush/clear` ran (see **Resolved 4**) |

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

**Branch:** `fix/hib-jboss-logger-base-intercept`. **Continues [HIB-CV-11](../../../apps/hibernate-orm/cratonvm-bug-reports/HIB-CV-11-jboss-logging-testable-provider.md)**, which made `Logger.getLogger(...)` correctly return `hibernate-testing`'s `DelegatingLogger` but left "1 residual" (UniqueConstraintBatchingTest) — this is that residual, plus the bag test.

Both tests assert on **emitted log messages** via the testing infra (`@LoggingInspections`/`MessageKeyWatcher`, `LoggerInspectionExtension`/`Triggerable`). That infra works by installing a `DelegatingLogger` whose **`doLog`/`doLogf` override** records each event to registered `LogListener`s. Two native short-circuits in CratonVM bypassed that override, so **every watcher saw zero events** — the assertions then read the wrong value with no native frame to point at. Neither test's "likely area" (JDBC batch, collection bookkeeping) was the actual cause.

1. **`logmanager.rs` "Round 92"** registered natives on the abstract `org/jboss/logging/Logger` for `info/warn/error/fatal/debug/trace` (+ `f`/`v`/fqcn variants), emitting straight to stderr. That short-circuited virtual dispatch to the concrete subtype's `doLog`/`doLogf`, so `DelegatingLogger.doLog` never ran → no interception. This drove `UniqueConstraintBatchingTest` (`SQLExceptionLogging.ERROR_LOG.warn(...)` event swallowed) and the `testMergeDetachedCollectionWithQueuedOperations` watchers.
   **Fix:** gated the Round-92 block behind opt-in `CRATONVM_JBOSS_LOGGER_BASE_EMIT=1` (default OFF → real bytecode dispatches to the real `doLog`). WildFly boot visibility is preserved by the pre-existing **Round-90** `doLog`/`doLogf` intercepts on the concrete backend subclasses (`JBossLogManagerLogger`, …).

2. **`jboss_msc.rs` "R63"** registered `DelegatingBasicLogger.isTraceEnabled/isDebugEnabled/isInfoEnabled` to **always return `false`** (to avoid NPEs when WildFly's synthetic backfills leave `this.log` null). But `AbstractPersistentCollection.logDiscardedQueuedOperations()` only logs `HHH90030006` inside `if (COLLECTION_LOGGER.isDebugEnabled())` — so on rollback the message (and its watcher) was skipped, failing `testCollectionWithQueuedOperationsOnRollback`. The test's `log4j2.properties` sets `org.hibernate.orm.collection=debug`, so the real answer is `true`.
   **Fix:** these natives now **delegate to `this.log.isXEnabled()` when `this.log` is non-null**, returning `false` only when it is null (preserving the WildFly null-safety).

**Verified** vs HotSpot (binary `cratonvm-hiblog.exe`, JIT-on and `--nojit`): UniqueConstraintBatchingTest `ok=1`, DetachedBagDelayedOperationTest `ok=2`. No regression in the HIB-CV-11 log-inspection tests (`BootLoggingTests`, `SessionFactoryNamingTests` 5/5, `AnyTypeFlushToLoggableStringTest`, `ImmutableEntityUpdateQueryHandlingModeWarningTest`). Opt-out `CRATONVM_JBOSS_LOGGER_BASE_EMIT=1` restores the old short-circuit. Other full-suite log-assertion tests (e.g. `@MessageKeyInspection`/`Triggerable` users) likely benefit too.

## Resolved 2 (2026-06-19) — `Throwable.addSuppressed` silently no-op'd (suppressedExceptions left null)

**Branch:** `fix/throwable-suppressed-init`. Fixes `LoaderWithInvalidQueryTest` (under nojit).

The test builds a SessionFactory whose entity has two invalid named queries; Hibernate's
`NamedQueryValidationException` aggregates each error via `Throwable.addSuppressed(...)`, and the test
asserts `rootCause.getSuppressed().length == 2`. On CratonVM it was `0` — and a one-line probe showed the
primitive itself was broken: `new Exception(); e.addSuppressed(a); e.addSuppressed(b); e.getSuppressed().length`
returned **0** (HotSpot: 2).

**Root cause:** CratonVM shadows `Throwable.<init>` with native constructors
(`native_exc_init_message`/`_noargs`/`_cause`, registered by `register_throwable_subclass_natives` for the
whole Throwable family). They mirror `detailMessage` and the `cause = this` sentinel and capture the stack
trace — but never initialized `suppressedExceptions`, which the real JDK field initializer sets to
`SUPPRESSED_SENTINEL`. `Throwable.addSuppressed` treats a **null** `suppressedExceptions` as "suppression
disabled" and returns immediately, so every `addSuppressed` was silently dropped and `getSuppressed()` was
always empty. (Reflection confirmed: `cause == this` ✓ but `stackTrace == null` and
`suppressedExceptions == null` after the ctor.) This also silently broke **try-with-resources** suppressed
exceptions everywhere.

**Fix** (`../../../../native-builtins/src/lang_misc.rs`): `capture_throwable_trace` (the shared chokepoint for all
exception-init natives) now mirrors `suppressedExceptions = SUPPRESSED_SENTINEL` via a new
`init_suppressed_sentinel`, reading the real static so `addSuppressed`/`getSuppressed`'s
`== SUPPRESSED_SENTINEL` identity checks hold. It only writes when the field isn't already a non-null list
(an unset reference slot reads back as `Int(0)`, not `Object(None)` — the guard accounts for that), so a
re-entrant `fillInStackTrace()` never clobbers an `addSuppressed`-populated list.

**Verified** vs HotSpot: primitive `getSuppressed().length == 2`; the LoaderProbe bootstrap repro shows
`root.getSuppressed().length == 2` with both messages; try-with-resources reports `suppressed=2` matching
HotSpot — all under JIT-on **and** `--nojit`. `LoaderWithInvalidQueryTest` `ok=1` under `--nojit`. No
regression in UniqueConstraintBatchingTest / DetachedBagDelayedOperationTest.

**Caveat:** `LoaderWithInvalidQueryTest` still FAILs under **JIT** — but with a `TimeoutException` (>120s),
not the suppressed assertion. The named-query errors are produced correctly; the EMF build's ANTLR HQL
parse is pathologically slow under CratonVM JIT and trips the test's own `@Timeout(120s)`. Separate,
pre-existing JIT-perf issue (same family as the ANTLR `computeTargetState` blowup), unrelated to suppressed
exceptions (the primitive is correct under JIT — see probes above).

**Still open (separate root causes):** `EntityGraphBatchSizeTest`
(both entity-graph fetch semantics); `ClassLoaderServiceImplTest` = HIB-CV-16. Plus the
JIT-only ANTLR-HQL-parse slowness above.

## Resolved 3 (2026-06-19) — `new HashMap<>(src)` serialized `loadFactor=0.0` + zero entries

**Branch:** `fix/hashmap-serialization`. Fixes `ImmutableWithAttributeConverterTest`.

`ExifConverter` is an `AttributeConverter<String, Exif>`, so the DB column type is `Exif` — a
`Serializable` value that wraps a `HashMap`. Hibernate Java-serializes it to the column and deserializes
on read, and that round-trip failed: `SerializationException: could not deserialize` caused by
`java.io.InvalidObjectException: Illegal load factor: 0.0`.

**Root cause:** CratonVM's native `HashMap.<init>(Map)` copy-constructor (`native_map_init_from_map`)
populates the synthetic bucket side-table but leaves the real-JDK `loadFactor`/`threshold`/`table` fields
unset (a top-level `new HashMap<>()` + `put` is real-backed and was fine; only the copy-constructor path
is native-backed). The inherited real `HashMap.writeObject` then read `loadFactor=0.0` and iterated an
empty real `table`, emitting a zero-load-factor, zero-entry stream. A hexdump cross-VM diff confirmed it
(CV 179 bytes vs HotSpot 204; `3f400000`→`00000000` for loadFactor, entries dropped). This silently broke
**any** object graph holding a `new HashMap<>(src)`.

**Fix** (`../../../../native-collections/src/lib.rs`): register native `HashMap.writeObject`/`readObject` (mirroring
the existing `TreeSet` pattern) that drive the stream from `collect_entries_any` — which reads BOTH
native-backed maps (bucket reader) and real-backed maps (`entrySet().iterator()` fallback) — and ensure a
valid `loadFactor` before `defaultWriteObject`. Capacity is taken from the real `table` length when present
so a real-backed map stays byte-identical to the inherited method.

**Verified** vs HotSpot: the `Exif` now round-trips and CratonVM's bytes (204) are byte-identical to
HotSpot and read back on HotSpot; `ImmutableWithAttributeConverterTest` `ok=2`. No regression — real-backed
HashMap (191B), LinkedHashMap (230B), the copy-constructor map, and the empty map all serialize
byte-identically to HotSpot.

**Still open:** `EntityGraphBatchSizeTest`; `ClassLoaderServiceImplTest` = HIB-CV-16. (`DetachedPreviousRowStateTest` is now **FIXED** — see **Resolved 4**.)

## Resolved 4 (2026-06-19) — CratonVM's synthetic `Stream` was eager; `StreamSupport.stream(spliterator).forEach` is now lazy

**Branch:** `feat/lazy-stream-pipeline`. Fixes `DetachedPreviousRowStateTest`.

The test does `getResultStream().forEach(ld -> { assertTrue(em.contains(ld.description.product)); em.flush(); em.clear(); })`. Two rows share one `Product`. On HotSpot the stream is lazy: row 2 is fetched *after* row 1's `em.clear()`, so the shared Product is re-managed → `contains==true`. On CratonVM the nested Product was detached mid-iteration. **Root cause:** CratonVM's synthetic `java/util/stream/Stream` is array-backed and eager — `StreamSupport.stream(realSpliterator,false)` (`service_loader.rs`) drained the spliterator into an `Object[]` up front, so the whole pipeline ran stage-by-stage (all `tryAdvance`, then all `map`, then all `forEach`) **before** the terminal consumer. Minimal proof: `StreamSupport.stream(customSpliterator,false).forEach(g)` prints, on HotSpot, interleaved `tryAdvance/accept` per element; on CratonVM it printed **all** tryAdvance, then **all** accept — so `em.flush()/clear()` ran only after every row was buffered.

**Fix** (make the common case lazy without rearchitecting the eager pipeline): `StreamSupport.stream(realSpliterator,false)` now stashes the spliterator in a new lazy slot (stream field 2) instead of draining; `Stream.forEach`, when the stream is still lazy (no intervening op materialised it), drives the spliterator one element at a time via `tryAdvance(consumer)`; and a single materialisation point — `stream_elements` is now `&mut` and drains the lazy spliterator into field 0 — means every other op (map/filter/collect/count/toArray/reduce/…) transparently sees the full list. A stream with an intermediate op materialises at that op and runs eagerly thereafter (correct results, just not interleaved — only a no-op forEach with side effects needs interleaving).

**Verified** vs HotSpot: no-op forEach is interleaved; `DetachedPreviousRowStateTest` `ok=1` (JIT-on and `--nojit`); a broad stream-ops regression (Collection.stream + StreamSupport `map/filter/collect/count/sorted/distinct/reduce/toArray/anyMatch/findFirst/flatMap/iterate/generate/parallel/peek/IntStream`) is byte-identical; the previously-fixed Hibernate tests still pass. (Pre-existing, NOT from this change — both reproduce on the pre-change binary: `IntStream.boxed()` returns empty; reusing a consumed synthetic stream doesn't throw `IllegalStateException`.)

## Root-caused but deep (2026-06-19) — the remaining entity-graph test

### `EntityGraphBatchSizeTest` — eager element-collections force per-row instead of deferring
Both methods fail at `assertSelectCount("GraphBatchBook_batchedTags", 1)` with `expected 1 but was 3`. Eager **entity** graph-batching works (`batchedAuthor` → 1 `where id in (?,?,?)`), and a persister-level `@BatchSize` lazy collection batches fine — but an **eager element-collection under a load-graph** is force-initialized **per row** instead of being deferred to `endLoading` after all keys are queued, so each `batchedTags` issues its own un-batched `where ..._id=?` (3 selects) instead of one batched `in (?,?,?)`. The behavioral boundary is sharp: `SelectEagerCollectionInitializer.initializeInstanceFromParent` (immediate `forceInitialization`) vs `resolveInstance` (deferred via `addNonLazyCollection`) — CV takes the per-row force path. **Fix direction:** the result-graph eager-collection deferral state machine; needs runtime instrumentation to pin the single diverging primitive.
