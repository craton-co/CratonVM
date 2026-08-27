# `hql.ASTParserLoadingTest` on MySQL — 1447 s against HotSpot's seconds, and 21 real failures on a FRESH database

## Status

**OPEN, 2026-08-27.** Two separate residuals, both of them previously hidden
inside the stale-schema cascade that
`fixed-suite-bugs/hibernate/mysql-cross-class-stale-schema-shared-worker-db-20260822.md`
root-caused and fixed. Neither is a schema artifact; both reproduce on a
brand-new database with nothing else on it.

The class matters out of proportion to its size, because it is the class whose
slowness *caused* that cascade: it exceeds the suite's 300 s wall cap, is
killed, and — until the harness fix — left its 54 tables behind for every later
class that shares a table name.

## The measurement

One class, one fresh MySQL 8.0.46 database each, same host, same
`common.args`, same MySQL dialect/driver/credentials:

| | tests | result | wall |
| --- | ---: | --- | ---: |
| HotSpot 25.0.3+9 | 106 found, 104 started | **104 ok, 0 failed** | seconds |
| CratonVM (ZGC) | 106 found, 104 started | **83 ok, 21 failed** | **1 447 s** |

Both left `tables left: 0`, which is what rules the schema out: the class cleans
up after itself on both VMs when it is allowed to finish.

## Residual 1 — ~25x slower, which is what puts it over the cap

1447 s against seconds is not a margin, and the suite's `--timeout 300` is not
an unreasonable cap for a class HotSpot finishes in single digits. Raising the
cap in `class-overrides.tsv` would convert the `HANG` into a very slow `FAIL`
and stop it poisoning its shard — the harness fix already stops the poisoning —
but it would not touch the 1447 s.

**Not yet established**: where the time goes. This class is the classic
`Animal`/`Human`/`Zoo` HQL model and drives a large number of distinct HQL
parses and executions, so the plausible buckets are HQL parsing, JDBC round
trips, and the JIT never getting warm across 104 short tests — and nothing here
distinguishes them. `hql.HqlParserMemoryUsageTest` is on record failing a
parser-memory budget (629 MB against 256 MB) and `HqlParseStress` /
`HqlParamBindProbe` already exist in `apps/hib-suite-runner/`, so the parser is
the first bucket to price, not the assumed one.

## Residual 2 — 21 of 104 tests fail where HotSpot passes all 104

These are the ones the cascade has been hiding: on a shared worker database the
class was killed at 300 s and recorded `HANG`, so its own test failures were
never reported at all. `HANG` destroys the per-test breakdown exactly the way a
netty cap did in
`fixed-suite-bugs/netty/ssl-parameterized-classes-exceed-180s-timeout-masking-real-failures-20260826.md`.

**Not yet triaged.** 21 failures against a HotSpot control of zero is a real
signal, but nothing here says whether it is 21 defects, one defect with 21
faces, or a mix — and the shapes have not been read yet. That is the first job:
attribute them by exception and frame before assuming a common cause, because
the last three times this family was counted rather than read, the count was
the wrong instrument.

## Why this is not simply "another 122-class MySQL failure"

The 2026-08-24 run recorded this class `HANG`, not `FAIL`, so it is not one of
the 122. It is upstream of ~45 of them: on its own shard, 45 classes failed
after it and 17 before.

## Repro

```bash
cd apps/hib-suite-runner
# a genuinely fresh database, or the result means nothing
docker exec mysql mysql -uroot -p... -e \
  "DROP DATABASE IF EXISTS hib_probe_a; CREATE DATABASE hib_probe_a; \
   GRANT ALL ON hib_probe_a.* TO 'hibernate_orm_test'@'%';"

URL="jdbc:mysql://localhost/hib_probe_a?allowPublicKeyRetrieval=true&useSSL=false"
COMMON=( -Djava.awt.headless=true
         -Dhibernate.dialect=org.hibernate.dialect.MySQLDialect
         -Dhibernate.connection.driver_class=com.mysql.cj.jdbc.Driver
         "-Dhibernate.connection.url=$URL"
         -Dhibernate.connection.username=hibernate_orm_test
         -Dhibernate.connection.password=hibernate_orm_test
         -Dcraton.batch=1 CratonRunner
         org.hibernate.orm.test.hql.ASTParserLoadingTest )

java @common.args "${COMMON[@]}"                      # 104 ok, seconds
cratonvm -XX:+UseZGC --java-home <jdk25> @common.args "${COMMON[@]}"   # 83 ok, 21 failed, ~1447 s
```

`hibernate.properties` currently points at **H2**, which is why every MySQL
sysprop above is passed explicitly; a run that reports `found=106 ok=0` in ~10 s
is that file, not this defect.

## Related files

- `apps/hibernate-orm/hibernate-core/src/test/java/org/hibernate/orm/test/hql/ASTParserLoadingTest.java`
- `apps/hib-suite-runner/class-overrides.tsv` (no entry for this class today)
- `apps/hib-suite-runner/HqlParseStress.java`, `HqlParamBindProbe.java`
- `fixed-suite-bugs/hibernate/mysql-cross-class-stale-schema-shared-worker-db-20260822.md`
  — the cascade this class caused, and the harness fix that ended it
