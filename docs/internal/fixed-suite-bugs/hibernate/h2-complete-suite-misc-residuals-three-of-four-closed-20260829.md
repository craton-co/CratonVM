# H2 complete-suite "four unrelated findings" — three were not findings

**Retired 2026-08-29**, replacing
`docs/known-issues/hibernate/h2-complete-suite-misc-residuals-20260829.md`.
That page grouped four single-class failures from the 4548-class H2 run and said
of all four: "None cross-checked against HotSpot yet. None root-caused." All
four now have the A/B it named as missing. Three are closed here. The fourth is
a real CratonVM defect and moves to its own page,
`docs/internal/fixed-suite-bugs/hibernate/jpalargeblob-random-state-side-table-FIXED-20260830.md`,
because it has a named mechanism and deserves not to be filed under
"miscellaneous".

Every row below is one execution per arm through
`apps/hib-suite-runner`'s `MethodRunner`, JDK 25.0.3+9-LTS Temurin, CratonVM
`dev@4080c8706`, on the Windows host whose OS locale is Russian.

## 1. `UniqueConstraintBatchingTest.testBatching` — the HOST LOCALE again

The open page had only `expected: <1> but was: <0>` and asked what the `1`
represents. It is a count of intercepted log messages:

```java
triggerable = logInspection.watchForLogMessages( "Unique index" );
...
catch (PersistenceException e) {
    assertEquals( 1, triggerable.triggerMessages().size() );
    assertTrue( triggerable.triggerMessage().startsWith( "Unique index or primary key violation" ) );
}
```

The `PersistenceException` IS thrown — the catch block is what runs — so the
constraint violation happened and the batching worked. What did not happen is
the English message. **H2 localizes its own `DbException` text from
`Locale.getDefault()`**, and on this host the log line reads:

```
WARN .hibernate.orm.jdbc.error SqlExceptionHelper:144 -
  Нарушение уникального индекса или первичного ключа: "PUBLIC.UNIQUEWITHINHERITED ..."
```

`watchForLogMessages("Unique index")` matches nothing, so the count is 0.

| VM | `-Duser.language=en -Duser.country=US` | result |
|---|---|---|
| real HotSpot | no | **FAIL** `ok=0 failed=1` |
| real HotSpot | yes | PASS `ok=1 failed=0` |
| CratonVM | no | **FAIL** `ok=0 failed=1` |
| CratonVM | yes | PASS `ok=1 failed=0` |

Not a CratonVM defect, and **already fixed**: the two locale rows landed in
`apps/hib-suite-runner/required-sysprops.tsv` earlier the same day for
`FunctionTests`/`StandardFunctionTests.testFormat`. This is the third class the
same missing sysprop was costing, and the first one whose failure did not look
like a locale problem at all — the assertion is on a COUNT, and the localized
string never appears in the failure message. See
`functiontests-format-day-name-was-the-host-locale-20260829-NOT-A-CRATONVM-BUG.md`.

**What generalises:** a `<1>` vs `<0>` on a log-watcher assertion is a
string-matching failure wearing a counter's clothes. Read what the watcher
matches before reading anything else.

## 2. `PackagedEntityManagerTest.testExcludeHbmPar` — the class never ran on H2

The open page read the error as "a generated-sequence table that schema export
should have created isn't present", and offered "an H2-specific
schema-generation gap" as one explanation. The wording rules that out on its
own: `ERROR: relation "caipirinha_seq" does not exist` is **PostgreSQL's**
sentence. H2 says `Table "X" not found`.

The class builds a JAR from `hibernate-core/target/bundles/excludehbmpar/` and
boots an EMF from the persistence.xml inside it. That file was filtered for
Postgres at fixture-build time:

```xml
<property name="hibernate.dialect" value="org.hibernate.dialect.PostgreSQLDialect"/>
<property name="hibernate.connection.driver_class" value="org.postgresql.Driver"/>
<property name="hibernate.connection.url"
          value="jdbc:postgresql://localhost/hibernate_orm_test_$worker?..."/>
```

A persistence unit carries its own connection settings, so `hibernate.properties`
— which is H2 — does not reach it. **The class targets Postgres whichever
database the suite is configured for.** It was never an H2 result, and the
`caipirinha_seq` failure is a stale worker schema in that Postgres, which is the
same species the open page's own footnote already attributes the Postgres
suite's 931 failures to (`DbReset.java`).

Measured with no Postgres reachable, both VMs, same arm:

| VM | result |
|---|---|
| real HotSpot | **FAIL** — `PSQLException: Connection to localhost:5432 refused` |
| CratonVM | **FAIL** — `PSQLException: Connection to localhost:5432 refused` |

Byte-identical cause. Not a CratonVM defect.

**What generalises:** the error's DIALECT is evidence about which database
answered, and it is available before any A/B. A Postgres sentence in an H2 run
means the class is not reading the suite's database configuration.

## 3. `HqlParserMemoryUsageTest.testParserMemoryUsage` — not a new finding

This is the already-tracked open page
`docs/known-issues/hibernate/hibernate-orm-hql-parser-memory-overhead-20260817.md`,
re-observed. That page records `~627,000 KB` against a 256 MiB budget; the
"new" finding reports `630,335 KB`. Same measurement, same class, same
mechanism, twelve days apart.

Re-confirmed here: real HotSpot PASSES it in `test_ms=2558`. It is also already
listed on
`docs/known-issues/hibernate/hibernate-and-hibernate-reactive-not-cratonvm-bugs.md`
as the one entry on that page which is a genuine open CratonVM characteristic
rather than a host artifact, re-measured 2026-08-24.

Filed as a fourth of a new page it was the sole subject of an older one — the
cost of grouping by "single class with no obvious cluster-mate" rather than
grepping the docs tree for the class name first.

## 4. `JpaLargeBlobTest.jpaBlobStream` — a real defect, moved not closed

HotSpot passes in `test_ms=7078`. CratonVM does not time out at 120 s so much as
take **312 s**: the `@Timeout(120)` is a method-level JUnit annotation that no
runner property can widen, so the run is reported as a timeout while the work
continues to completion. Two independent mechanisms, both measured, in
`docs/internal/fixed-suite-bugs/hibernate/jpalargeblob-random-state-side-table-FIXED-20260830.md`.
