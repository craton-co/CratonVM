# Spring-suite 2026-06-19 — OPEN candidate defects (pending HotSpot triage)

These surfaced while running the full `apps/spring-framework` suite under CratonVM
(baseline `c4536b94`). Unlike [`springsuite-0619-unsafe-offheap-directbuffer`](springsuite-0619-unsafe-offheap-directbuffer.md)
and [`springsuite-0619-getbeanclassname-bean-filter`](springsuite-0619-getbeanclassname-bean-filter.md)
(both root-caused, triaged HotSpot-OK, and **FIXED on dev**), the items below are
**candidates**: clustered by failure signature but **not yet individually HotSpot-triaged**
and most still need a runtime stack to confirm the root cause. Counts are from the clean
shard run (~631 classes, through spring-jdbc); the suite did not finish (paused to land the
two fixes). Do not treat these as confirmed CV-unique until triaged.

## Method / status
- Run harness: `apps/spring-framework` via `spring-suite/run-spring-cratonvm.sh` (KRun, one VM
  per batch, crash-recovery to per-class isolation). Triage script: `spring-suite/triage-spring.sh`
  (re-runs suspects under HotSpot JDK 25 to drop SAME-AS-HotSpot).
- The two FIXED bugs already cover the **two largest** clusters: ~100+ `Target object must not
  be null` (bug-B) and the `Unsafe … not in any live arena` cluster (bug-A). Re-run the suite on
  the fixed binary before triaging the below — several may already be gone or reduced.

---

## C/F — `ReactiveAdapterRegistry$MutinyRegistrar` NCDFE / `ExceptionInInitializerError`  *(~40 classes — strongest new candidate)*
**Signature:** `NoClassDefFoundError: org/springframework/core/ReactiveAdapterRegistry$MutinyRegistrar`
(102 occurrences) and `ExceptionInInitializerError` on the same reactive/messaging/rsocket
classes (http.client, http.codec, messaging.handler.*.reactive, messaging.rsocket.*).
**Hypothesis (needs confirmation):** Spring guards optional integrations with
`ClassUtils.isPresent("io.smallrye.mutiny.Multi", cl)`. If CratonVM's `isPresent` /
`Class.forName` returns a **false positive** (or fails to throw `ClassNotFoundException`) for an
absent optional dependency, Spring proceeds to load the guarded nested `MutinyRegistrar`, which
references the absent `io.smallrye.mutiny.*` types → `NoClassDefFoundError`. This is the
**inverse** of bug-B (both are class-presence-detection defects). Also seen with
`reactor/core/scheduler/BoundedElasticScheduler` (8) and
`MethodValidationInterceptor$ReactorValidationHelper` (6).
**Next step:** repro `new ReactiveAdapterRegistry()` standalone with reactor-but-not-Mutiny on
the classpath; check `ClassUtils.isPresent` against a known-absent class vs HotSpot.

## D — XML `BeanDefinitionParsingException: "Unexpected failure during bean definition parsing"`  *(~19 classes)*
aop.framework.autoproxy, context.support, jdbc.object, and the whole `jmx.export.assembler.*`
group (7 classes fail identically → likely one JMX-XML root cause). Needs the wrapped cause
(stack) to localize — candidate areas: XML/SAX parsing, a namespace handler, or downstream of
the bean-class resolution path.

## E — XML `"Unnamed bean definition specifies neither 'class' nor 'parent' nor 'factory-bean'"`  *(~35: 23 ParsingException + 12 StoreException)*
Bean-name generation fails because the parsed definition has no class. Possibly bug-B/bug-D
adjacent (a null/var bean class name leaking into name generation). Re-check after the bug-B fix.

## G — spring-jdbc mass TIMEOUT  *(~25 classes — needs isolation re-verify)*
Nearly the entire `jdbc.core` / `jdbc.object` / `jdbc.datasource` module timed out
(`JdbcTemplateTests`, `SimpleJdbcCallTests`, `SqlQueryTests`, `DataSourceTransactionManagerTests`, …),
plus `UncategorizedScriptException: Failed to execute database script` (9). Could be a systemic
embedded-DB/`DataSource` init hang **or** the known interpreted-instance-method perf pathology
amplified under the (then 3-way) parallel run. **Must be re-verified one-at-a-time on a quiet
machine** before classifying as a true hang vs slow.

## H — scheduler `StringIndexOutOfBoundsException`  *(2 classes, small/isolated)*
`scheduling.concurrent.ConcurrentTaskSchedulerTests` and `ThreadPoolTaskSchedulerTests`
(`executeRunnable()`, `submitCompletable*()`) throw `StringIndexOutOfBoundsException: null` —
a string-parse OOB on a scheduler path (thread-name / duration?). Needs a stack.

---

## A2 — Netty refcount `ArrayIndexOutOfBoundsException` (off-heap `retain/release`)  *(residual of bug-A, OPEN)*
After the bug-A arena fix, `PooledDataBufferTests.retainAndRelease()` / `tooManyReleases()` still
fail — but now with `ArrayIndexOutOfBoundsException` (not the arena IAE). The off-heap byte access
works; the **reference-counting** path is the new culprit (likely Netty's
`AtomicIntegerFieldUpdater`-backed `refCnt`). Distinct from bug-A. Needs a stack.

## B2 — CGLIB method-injection / `@Configuration` subclass instantiation returns null  *(residual of bug-B, OPEN)*
After the bug-B filter fix, `LookupMethodTests` still fails "Target object must not be null": the
bean instance is null even though the class name now resolves. HotSpot instantiates the CGLIB
subclass (`…AbstractBean$$SpringCGLIB$$0`); CratonVM's CGLIB **method-injection / config-class
subclass** instantiation does not. Likely the real root of much of the ~100+ "Target object must
not be null" cluster (most are `@Configuration` proxies). High value, deeper. Needs a stack at
`CglibSubclassingInstantiationStrategy.instantiateWithMethodInjection`.

## Already-tracked (seen again here, not new)
- generics `FieldTypeSignature` CCE (9) — `bug-05` cluster.
- JUnit `@Timeout` "called invocation multiple times" (14) — [`spring-bug-04`]; note the CCE half
  landed on dev (`fc20e970`) — re-check whether this masking symptom remains.
- Groovy script compile failures (15) — [`spring-bug-11`](spring-bug-11-groovy-and-scheduler-crashes.md).
- JUnit-platform `getId()` no-Code / `delegate is null` LOADERR — [`spring-bug-10`](spring-bug-10-junit-platform-execution-loaderr.md) family.

## Environmental (HotSpot also fails — NOT CratonVM bugs)
- `NoClassDefFoundError: org/springframework/aot/test/generate/TestGenerationContext` (51),
  `MockSpringFactoriesLoader`, `TestCompiler` — missing AOT test-fixture jars on the harness classpath.
- `ConnectException: localhost:<port>` (12) — tests needing a live server.
