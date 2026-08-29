# Multi-tenancy (H2): the six failures were the fixture URL, not the VM — RETIRED 2026-08-29

**Verdict: NOT a CratonVM defect.** Byte-identical failures under stock
HotSpot 25. Root cause is one substring missing from an untracked generated
fixture file. Fixed by restoring the upstream value; all 12 tests across the
4 classes now pass on **both** VMs.

Retires `known-issues/hibernate/multitenancy-h2-tenant-isolation-primary-key-collision-20260829.md`,
which filed the cluster with the correct open question — "genuine VM defect,
or an H2-vs-Postgres harness gap?" — and named the HotSpot A/B as the fastest
way to settle it. That A/B had not been run. It takes one command and answers
it outright.

## The defect

The four classes derive their per-tenant JDBC URL by **substring replacement**
on whatever `hibernate.properties` carries:

```java
// DatabaseMultiTenancyTest.java:18 (also DatabaseTimeZoneMultiTenancyTest:205,
// MultiTenantConnectionProviderFromBeanContainerTest:86)
protected String tenantUrl(String originalUrl, String tenantIdentifier) {
    return originalUrl.replace( "db1", tenantIdentifier );
}
```

Upstream's Gradle build substitutes `@jdbc.url@` in
`hibernate-core/src/test/resources/hibernate.properties` with the H2 value from
`local-build-plugins/src/main/groovy/local.databases.gradle:23`:

```
jdbc:h2:mem:db1;DB_CLOSE_DELAY=-1;LOCK_TIMEOUT=10000;DB_CLOSE_ON_EXIT=FALSE
```

This host's copy — `hibernate-core/target/resources/test/hibernate.properties`,
which is **generated and untracked**, hand-edited to switch the suite between
H2 / Postgres / MySQL — read:

```
jdbc:h2:mem:db_$worker;DB_CLOSE_DELAY=-1;LOCK_TIMEOUT=10000
```

There is no `db1` substring in `db_$worker`. So `replace("db1", tenant)` is a
**no-op**: `front_end` and `back_end` both resolve to the same in-memory
database. Each test then inserts `Person(id=1, name='John Doe')` into what it
believes are two separate tenants, and the second insert collides:

```
Unique index or primary key violation: "PUBLIC.CONSTRAINT_8 PRIMARY KEY ON
PUBLIC.PERSON(ID) ( /* key:1 */ CAST(1 AS BIGINT), 'John Doe')"
```

`testChangeTenantWithoutConnectionReuse` fails one step earlier and more
legibly — `AssertionError: expected:<1> but was:<2>` at
`AbstractMultiTenancyTest:159`, because the "other" tenant's query sees both
rows. That assertion is the clearer tell that the two tenants share a database;
the PK violation is the same fact reported by H2 instead of by JUnit.

Note the `$worker` token is itself inert under this harness: `run-hib.sh` forks
one VM per class, so `GradleParallelTestingResolver.getWorkerID()` — which keys
its sequence file by the JVM's parent PID — always resolves to worker 1. The
URL was effectively `jdbc:h2:mem:db_1`.

## The A/B that settles it

Same `common.args`, same fixture, stock HotSpot 25 (Temurin 25.0.3.9):

| | before (`db_$worker`) | after (`db1`) |
|---|---|---|
| HotSpot `DatabaseMultiTenancyTest` | `found=3 ok=1 failed=2` | `found=3 ok=3 failed=0` |

The failure shape on HotSpot is byte-identical to the one the known-issues page
recorded for CratonVM, down to the `CONSTRAINT_8` name and the localized H2
message. That single run is the whole verdict: nothing here is VM-specific.

## After the fix — all four classes, both VMs

`hibernate.connection.url` restored to the upstream value.

| Class | HotSpot 25 | CratonVM `dev@84a98929e` |
|---|---|---|
| `multitenancy.DatabaseMultiTenancyTest` | 3/3 | 3/3 |
| `multitenancy.DatabaseTimeZoneMultiTenancyTest` | 1/1 | 1/1 |
| `multitenancy.beancontainer.MultiTenantConnectionProviderFromBeanContainerTest` | 4/4 | 4/4 |
| `multitenancy.beancontainer.MultiTenantConnectionProviderFromSettingsOverBeanContainerTest` | 4/4 | 4/4 |

`failed_classes=0` on both arms. 12 tests, 0 failures, 0 aborted, 0 skipped.

## What keeps it from recurring

`hibernate.properties` has the same problem `common.args` has and that
`required-sysprops.tsv` exists to solve: it is generated, untracked, edited by
hand, and has no authoritative copy — so a lost token is invisible until it
surfaces as a test failure that looks like a VM bug. `run-hib.sh` now carries
`check_h2_fixture_url()`, which locates `hibernate.properties` on the classpath
the forks actually consume and, **when the dialect is H2**, verifies the URL
still carries the `db1` token. It prints a WARNING naming the four classes and
the upstream value, and records `h2-url=<state>` in the mode header and in
`SUMMARY.txt`, so the state is a measurement rather than an absence of one.
`HIB_NO_FIXTURE_CHECK=1` disables it for an A/B. Verified in both directions:
`ok (db1 present)` on the corrected file, and the full warning plus
`BROKEN: H2 url has no 'db1' token` when the old value is put back.

The check is advisory, not fatal: a Postgres or MySQL arm reports
`n/a (dialect=…)` and is untouched.

## Method note

The known-issues page named the decisive experiment in its own "Not yet done"
section and did not run it. A HotSpot arm on four classes costs ~9 s of wall
clock and converted an open triage question into a closed one-line config fix.
Run the control the page already asks for before spending a pass on the theory.
