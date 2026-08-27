# `hql.ASTParserLoadingTest`: the 25x was a HotSpot column that said "seconds", and the 21 failures do not reproduce

## Status

**RETIRED, 2026-08-27.** Neither residual survives being measured a second time.
Nothing in the VM was wrong; the page's own instrument was.

What is left of it is one real harness gap, now closed: the class needs a
per-class timeout floor, because it takes longer than the suite's flat 300 s cap
**on both VMs**.

## Residual 1 — "~25x slower" was a comparison against a number nobody measured

The page's table gave CratonVM `1447 s` and HotSpot `seconds`. Re-measured, one
fresh MySQL 8.0.46 database per arm, same box, same `common.args`, serial, on an
idle machine:

| | tests | result | wall |
| --- | ---: | --- | ---: |
| HotSpot 25.0.3+9 | 106 found, 104 started | 104 ok, 0 failed | **502 s** |
| CratonVM (ZGC, `--Xmx 1500m`) | 106 found, 104 started | 104 ok, 0 failed | **633 s** |

**1.26x, not 25x.** HotSpot does not finish this class in seconds; it finishes
it in eight and a half minutes, and it is over the suite's cap too.

### Where the time actually goes — the question the page left open

Not the JIT, not HQL parsing, not the collector. Both arms issue a
**byte-identical 8533 SQL statements**, and three quarters of them are DDL:

| statement | count |
| --- | ---: |
| `truncate table` | 5616 |
| `alter table` | 393 |
| `drop table` | 324 |
| `create table` | 162 |
| *(all DDL)* | *6495 of 8533 — 76%* |
| `select` / `insert` / `update` | 1958 |

That is the fixture clearing its ~54 tables after each of 104 tests. On InnoDB a
`TRUNCATE TABLE` is a tablespace drop-and-recreate — a metadata operation, not a
row delete — and 5616 of them is the wall. The two VMs issue the same statements
in the same order, so whatever the per-statement cost is, both pay it.

### And where the page's 1447 s came from

The wall scales with load on the **database**, not on the CPU. Six concurrent
copies of the class, each on its own fresh schema — which is what a six-way
shard run produces — measured **1452-1456 s** each in one batch and
**1486-1496 s** in a second. The page's headline number is what this class costs
under shard concurrency, compared against a HotSpot number that was not.

This is the failure mode
[`a-contended-host-inverts-an-ab`] exists for: the two rows of that table were
not taken under the same conditions, and the ratio between them was the finding.

## Residual 2 — 21 failures in 104, and 0 in 1352

The page reported `83 ok, 21 failed` and said the shapes had not been read yet.
They still have not, because they have not appeared:

| binary | runs | concurrency | result |
| --- | ---: | --- | --- |
| dev `fc51560b6` | 1 | serial | 104 ok, 0 failed |
| dev `fc51560b6` | 6 | six-way, fresh schema each | 104 ok, 0 failed (×6) |
| dev `fc51560b6` | 6 | six-way, fresh schema each | 104 ok, 0 failed (×6) |

**13 runs, 1352 tests, zero `@@TESTFAIL` lines.** Every run on a database
created for it and dropped after.

A single green run would prove nothing here — this class is on record as
nondeterministic twice over (`run-astparser-witness.sh` exists because of an
ANTLR moving-young misparse, `run-astparser-hunt.sh` because of an HQL
ordinal-parameter drop that reproduced about once in fourteen runs). That is why
the answer is a run count and not a run.

## The one real finding: the class needs a timeout floor

Both residuals were measurement, but the thing that made them invisible was not.
At 633 s (and up to ~1500 s under shard concurrency) against a flat 300 s cap,
the class is killed and recorded `HANG` — with `ms=0`, no `@@RESULT`, and **no
per-test breakdown at all**. That is exactly how a claim of "21 real failures"
could stand unexamined for a week: the run that would have listed them never
reported them.

`apps/hib-suite-runner/class-overrides.tsv` now carries

```
org.hibernate.orm.test.hql.ASTParserLoadingTest	3600	-
```

with the measurement above as its rationale, alongside the four classes already
there for the same reason. `run-hib.sh overrides` reports `state: 5 (loaded)`.

The floor is 3600 rather than ~1200 because of the concurrency scaling: the
number to design against is the six-way one, not the serial one.

## What this page was right about

That it mattered. It is the class whose 300 s kill caused the cross-class
stale-schema cascade in
`fixed-suite-bugs/hibernate/mysql-cross-class-stale-schema-shared-worker-db-20260822.md`
— killed at the cap, it left its 54 tables behind for every later class on the
shard sharing a table name, and 45 classes failed after it on its own shard. The
harness fix there (reset a shard's worker database after a kill) closed the
cascade; this floor stops the kill.

## Repro

```bash
docker exec mysql mysql -uroot -p... -e \
  "DROP DATABASE IF EXISTS hib_ast; CREATE DATABASE hib_ast; \
   GRANT ALL ON hib_ast.* TO 'hibernate_orm_test'@'%';"

URL="jdbc:mysql://localhost/hib_ast?allowPublicKeyRetrieval=true&useSSL=false"
COMMON=( -Djava.awt.headless=true
         -Dhibernate.dialect=org.hibernate.dialect.MySQLDialect
         -Dhibernate.connection.driver_class=com.mysql.cj.jdbc.Driver
         "-Dhibernate.connection.url=$URL"
         -Dhibernate.connection.username=hibernate_orm_test
         -Dhibernate.connection.password=hibernate_orm_test
         -Dcraton.batch=1 CratonRunner
         org.hibernate.orm.test.hql.ASTParserLoadingTest )

cd apps/hib-suite-runner
java @common.args "${COMMON[@]}"                                       # 502s
cratonvm -XX:+UseZGC --java-home <jdk25> --Xmx 1500m @common.args "${COMMON[@]}"  # 633s
```

A fresh database per arm is load-bearing: on a reused one this class is the
cascade above, not this measurement. `hibernate.properties` points at **H2**,
which is why every MySQL sysprop is passed explicitly — a run that reports
`found=106 ok=0` in ~10 s is that file, not this class.

The statement census that answers "where does the time go":

```bash
grep -A1 '^Hibernate: ' <run>.out | grep -v '^Hibernate: ' | grep -v '^--$' \
  | sed 's/^ *//' | awk '{print tolower($1), tolower($2)}' | sort | uniq -c | sort -rn
```

## Related files

- `apps/hib-suite-runner/class-overrides.tsv` — the floor, and the measurement
- `apps/hib-suite-runner/run-astparser-witness.sh`, `run-astparser-hunt.sh` —
  the two existing repeat harnesses, both built for this class's real
  nondeterministic defects
- `fixed-suite-bugs/hibernate/mysql-cross-class-stale-schema-shared-worker-db-20260822.md`
  — the cascade this class's kill caused
