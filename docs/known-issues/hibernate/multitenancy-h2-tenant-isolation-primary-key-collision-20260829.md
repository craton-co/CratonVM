# Multi-tenancy tests (H2): tenant switching doesn't isolate data — same PK collides across tenants

## Status
New finding, 2026-08-29, H2 complete-suite run. One shared root symptom
across 4 classes. Not yet confirmed CratonVM-specific (no HotSpot-on-H2 A/B
run) or narrowed to H2-vs-Postgres dialect behavior vs a real CratonVM
multitenancy-connection-provider defect.

## Symptom

All four classes fail with the identical shape — a primary-key collision on
data that should have landed in two logically separate tenant databases:

```
org.hibernate.orm.test.multitenancy.DatabaseMultiTenancyTest.testBasicExpectedBehavior
org.hibernate.orm.test.multitenancy.DatabaseMultiTenancyTest.testChangeTenantWithoutConnectionReuse
org.hibernate.orm.test.multitenancy.DatabaseTimeZoneMultiTenancyTest.testBasicExpectedBehavior
org.hibernate.orm.test.multitenancy.beancontainer.MultiTenantConnectionProviderFromBeanContainerTest.testBasicExpectedBehavior
org.hibernate.orm.test.multitenancy.beancontainer.MultiTenantConnectionProviderFromBeanContainerTest.testChangeTenantWithoutConnectionReuse
org.hibernate.orm.test.multitenancy.beancontainer.MultiTenantConnectionProviderFromSettingsOverBeanContainerTest.testBasicExpectedBehavior
```

```
org.hibernate.exception.ConstraintViolationException: could not execute batch
[Нарушение уникального индекса или первичного ключа: "PUBLIC.CONSTRAINT_8 PRIMARY KEY ON PUBLIC.PERSON(ID) ( /* key:1 */ CAST(1 AS BIGINT), 'John Doe')"
Unique index or primary key violation: ...; SQL statement:
insert into Person (name,id) values (?,?) [23505-240]] [insert into Person (name,id) values (?,?)]
```
(the Russian text is H2's own localized error message on this host's OS
locale, not part of the defect — it's the literal `PRIMARY KEY` violation
message.)

Each test inserts `Person(id=1, name='John Doe')` into what should be two
distinct tenant databases in turn; the second insert collides with the
first, meaning the tenant-connection-provider switch isn't actually routing
to a separate database/schema the second time.

## Not yet done — the open question this needs

Whether this is:
1. A genuine CratonVM defect in how the `MultiTenantConnectionProvider`
   mechanism resolves/switches connections, or
2. An H2-specific behavior gap — H2's tenant-per-database switching in this
   test harness may rely on something (e.g. distinct `jdbc:h2:mem:` URLs per
   tenant reliably producing isolated in-memory databases) that works
   differently than it does on Postgres, independent of CratonVM.

No HotSpot-on-H2 A/B has been run for any of these four classes yet — that's
the single fastest way to settle which of the two above this is, and should
be done before assuming either.

## Repro

```bash
cd apps/hib-suite-runner
cratonvm.exe --java-home <jdk25> -XX:+UseZGC <hibernate-orm classpath+args> \
  JUnitRunner org.hibernate.orm.test.multitenancy.DatabaseMultiTenancyTest
# hibernate.properties must be pointed at H2 (see hibernate.properties.bak-postgres-20260829
# for how to switch back if needed) — this reproduces on H2, not yet tried on Postgres.
```
