# Hibernate Reactive — the 30 s `@BeforeEach` cap vs CratonVM start-up time

**Date:** 2026-10-24
**Status:** open, performance (not a correctness difference from HotSpot)
**Found by:** the retirement of
`docs/internal/fixed-suite-bugs/hibernate-reactive/hibernate-reactive-single-jvm-execution-failures-20261020-RESOLVED.md`

## Symptom

Fork-per-class on Windows with Testcontainers, ~22-24 of 204 classes fail on CratonVM only (all three
GCs), HotSpot passes all:

```text
java.util.concurrent.TimeoutException: The test execution timed out. Make sure your asynchronous code
includes calls to either VertxTestContext#completeNow(), ... 
Suppressed: ... NullPointerException: ... SessionFactoryManager.getHibernateSessionFactory() is null
```

Examples: `SQLSelectTest`, `LockTimeoutTest`, `DirtyCheckingIT`, `EmbeddedIdWith*`, `SoftDelete*`,
`MutationDelegate*`, `RowIdUpdateAndDeleteTest`, `NoEntitiesTest`, `BasicTypesAndCallbacksForAllDBsTest`.

## Cause

`BaseReactiveTest.before()` carries `@Timeout(10 MINUTES)`, but vertx-junit5 5.1.5
(`VertxExtension.joinActiveTestContexts`) resolves `@Timeout` from the *test method* or the *class*,
never from the `@BeforeEach` method (read from the bytecode; identical on HotSpot), so `before()` is
capped at 30 s on both VMs. `before()` = Testcontainers start of Postgres + Hibernate
`SessionFactory` build. Timeline of `SQLSelectTest`, one run each (seconds from JVM start):

| event | HotSpot | CratonVM |
| :--- | ---: | ---: |
| `HHH000001` (Hibernate core loaded) | 2.7-4.4 | 12.5-18.0 |
| ryuk container up | 5.4-7.1 | 18.2-25.6 |
| Postgres container up | 6.4-8.3 | 33.4-41.5 |
| result | 13.9 | fail at ~42-48 (30 s cap) |

Live `--nojit` dumps show the worker interpreting Jackson serialization of the container-create
request (`BeanSerializer`, `AnnotatedMethod.getValue`, `Method.invoke`) and the credential-helper
round trip; with the JIT it is the same code, warmed up too late. What was ruled out: the Docker named
pipe (`AsynchronousFileChannel` ping 4-9 ms on both VMs), process spawn (`docker-credential-desktop`
~100 ms on both), `@Timeout` annotation reflection (identical on both), selector wake-up (NIO
loopback ping-pong is ~3x slower on CratonVM, no pathology).

## Evidence that nothing else hides behind it

The 25 distinct failing classes were re-run serially against an external Postgres
(`-DskipTestcontainers=true`, port 5432): **25 / 25 PASS** on default GC, and again on G1 and
Generational (26 / 26 with `MultithreadedInsertionWithLazyConnectionTest`), plus `TechEmpowerTest`
on all three GCs. With all 204 classes in one JVM (container started once) CratonVM default GC is
identical to HotSpot.

## Workarounds

* Pre-start the database (`-DskipTestcontainers=true` plus a Postgres on `localhost:5432`, user /
  password / db `hreact`), one DB per concurrent run: the port is fixed, so parallel arms must run
  serially or in separate network namespaces.
* One JVM for many classes: the container is static, so only the first class pays the start-up.
* `MultithreadedInsertionWithLazyConnectionTest` alone takes 552-623 s (HotSpot 34 s); it has its own
  900 s override in `apps/hibernate-reactive-suite-runner/class-overrides.tsv`.

## Linux, external DB, slow host (2026-10-24): the cap is hit without Testcontainers

A full fork-per-class interleaved run on a **4 vCPU / 7 GB, 2.0 GHz EPYC (IBPB) VM**
(`-DskipTestcontainers=true`, Postgres 18.4 on `localhost:5432`, `dev` @ `8658157896`) failed
`SoftDeleteCollectionTest`, `SoftDeleteConverterTest`, `SoftDeleteJoinedTest`,
`SoftDeleteSingleTableTest` and `SoftDeleteTablePerClassTest` on CratonVM only, each exactly one test
(the first one), and the failures held on a serial re-run with a 600 s wall timeout. Same signature
as above: `TimeoutException ... VertxTestContext#completeNow()` raised from
`VertxExtension.interceptBeforeEachMethod`, suppressed NPE from `cleanDb` because
`getHibernateSessionFactory()` is null. The second test then rebuilds the factory warm (~5 s) and the
remaining tests pass; the `relation ... already exists` DDL warnings after the failure are that
rebuild, harmless.

There is no container start-up here, so the whole 30 s goes to the *first, cold* `SessionFactory`
build. Timeline of `SoftDeleteCollectionTest` on that host (seconds after the first SLF4J line):

| event | CratonVM |
| :--- | ---: |
| `HHH000001` (`before()` running) | 8 |
| `HV000001` (Validator) | 26 |
| `HR000001` (Reactive) | 28 |
| `@@TESTFAIL` (30 s cap from `before()`) | 38 |
| second `HR000001` (warm rebuild) | 43 |

**Not a VM regression — a hardware-speed one.** The same five classes, same setup (external
Postgres 18.6, fork-per-class), on a 16-core EPYC-Turin host with `dev` @ `646cee1b86`
(the delta to `8658157896` is GC worker-join / JNI / Windows-attach only):

| class | CratonVM fast host | HotSpot fast host | CratonVM slow host | HotSpot slow host |
| :--- | ---: | ---: | ---: | ---: |
| `SoftDeleteCollectionTest` | PASS 14,606 | PASS 3,051 | FAIL 3/4 84,850 | PASS 18,328 |
| `SoftDeleteConverterTest` | PASS 11,732 | PASS 2,647 | FAIL 3/4 64,178 | PASS 15,234 |
| `SoftDeleteJoinedTest` | PASS 12,702 | PASS 2,796 | FAIL 3/4 60,089 | PASS 12,836 |
| `SoftDeleteSingleTableTest` | PASS 15,589 | PASS 4,784 | FAIL 5/6 65,304 | PASS 13,567 |
| `SoftDeleteTablePerClassTest` | PASS 20,259 | PASS 3,392 | FAIL 3/4 60,736 | PASS 15,516 |

HotSpot itself is ~4-6x slower on the small host, and CratonVM scales by the same factor
(`SoftDeleteCollectionTest`: 6.0x HotSpot, 5.8x CratonVM). The fast-host CratonVM times are also
*better* than the 2026-10-12 full interleaved run (`SoftDeleteCollectionTest` 26,526 ms then,
14,606 ms now), so start-up has improved, not regressed. CratonVM simply needs ~4-5x HotSpot's
cold-boot time, and on a 4-vCPU box that crosses 30 s; with only 4 cores the JIT's background
compile threads also compete with the Vert.x workers doing the boot. `SQLSelectTest` and
`MutationDelegateIdentityTest` sit right at the edge on that host (failed in the full run, passed
on the serial re-run at ~51 s for the whole class).

`vertx-junit5` 5.2.1 (the version on the classpath now) still hard-codes
`DEFAULT_TIMEOUT_DURATION = 30` with no system property, so neither `common.args` nor
`class-overrides.tsv` can lift it; only a `@Timeout` on the test class/method would.

Practical rule: do not use hosts below ~8 cores for fork-per-class hibernate-reactive comparisons,
or read the `SoftDelete*` / `SQLSelectTest` / `MutationDelegate*` first-test timeouts there as this
page, not as a regression.

## What would close it

CratonVM start-up of Jackson / Hibernate-boot-heavy code within roughly 3x of HotSpot, i.e.
`before()` under ~25 s. Tracked with the interpreter/JIT warm-up work; no VM correctness change is
needed.
