# hibernate-reactive suite — 3 GCs on MySQL, on the lambda deopt-resume fix (2026-08-22)

**Binary:** `C:/craton/CratonVM-hibfix-20260822/target/release/cratonvm-hibfix-3gc.exe`,
built at `dev` `bec3dca17` — i.e. including the
`try_lambda_site_direct_call` deopt-resume fix
(`docs/known-issues/hibernate/hib-reactive-3gc-run-regressions-20260820.md` §8).

**DB:** MySQL in Docker via Testcontainers, image
`container-registry.oracle.com/mysql/community-server:9.7.1` (the `FROM` in
`tooling/docker/mysql.Dockerfile`), **one container per class** —
`skipTestcontainers=false`, which is what gives per-class schema isolation and
is the same arrangement the 2026-08-20 Postgres run used.

**Harness:** `hibfix-hr-suite.sh` (a copy of `run-hibernate-reactive-suite.sh`
whose `COMMON` argfile is overridable via `HR_COMMON`) plus
`hibfix-common-mysql.args` (`common.args` with `-Ddb=PostgreSQL` →
`-Ddb=MySQL`). Full 249-entry `testlist.txt`, driver `hibfix-3gc-mysql.sh`.

---

## Pass 1 — full suite, three GC arms in parallel, 3 shards each

Nine concurrent forks, nine concurrent MySQL containers, `--timeout 240`.

| GC | PASS | FAIL | HANG | CRASH | NOTESTS | wall |
|---|---:|---:|---:|---:|---:|---:|
| ZGC (default) | 179 | 25 | 0 | 0 | 45 | 52m55s |
| G1 | 181 | 22 | 1 | 0 | 45 | 53m56s |
| Generational | 183 | 20 | 1 | 0 | 45 | 55m21s |

249 classes accounted for on every arm. `NOTESTS` = the harness's no-`@@RESULT`
bucket (45 here vs 44 on Postgres — one more class discovers zero tests on
MySQL).

**These FAIL counts are not VM results, and pass 2 is why.** Nine concurrent
MySQL container boots saturate the Docker daemon: three classes on the ZGC arm
fail with the literal `IllegalStateException: Could not find a valid Docker
environment` / `Previous attempts to find a Docker environment failed`, and
`FilterWithPaginationTest` — the class the §8 fix exists for — is one of them,
with `found=35 ok=0 failed=35`, i.e. every test in the class failing at setup.
The 2026-08-20 Postgres run hit the same signature once
(`UUIDAsBinaryTypeTest`); MySQL containers are heavier and it generalises.

## Pass 2 — the 28-class non-PASS union, at concurrency matched to a HotSpot control

Union of every non-PASS class across the three arms (28 classes), re-run
**one arm at a time, `--shards 2`, `--timeout 300`**, and — on the identical
list, argfile, shards and timeout — under real HotSpot
(`--hotspot`, Temurin 25.0.3).

| arm | PASS | FAIL | non-PASS classes |
|---|---:|---:|---|
| **HotSpot** | 27 | 1 | `DatabaseHibernateReactiveTest` |
| **ZGC (default)** | 26 | 2 | `DatabaseHibernateReactiveTest`, `MultithreadedInsertionWithLazyConnectionTest` |
| **G1** | 25 | 3 | + `techempower.TechEmpowerTest` |
| **Generational** | 23 | 5 | + `TechEmpowerTest`, `MultithreadedIdentityGenerationTest`, `SoftDeleteCollectionTest` (3/4 ok) |

`DatabaseHibernateReactiveTest` fails on HotSpot too, so it is not a CratonVM
defect (it is already documented as a host/environment class). Subtracting it:

| arm | CratonVM-only failures |
|---|---:|
| ZGC | **1** |
| G1 | **2** |
| Generational | **4** |

### Corrected full-suite figures

204 runnable classes per arm (249 minus 45 NOTESTS):

| GC | PASS | CratonVM-only failures |
|---|---:|---:|
| ZGC (default) | **202** | 1 |
| G1 | **201** | 2 |
| Generational | **199** | 4 |

For scale, the 2026-08-20 Postgres full run on the pre-fix binary was
188 / 190 / 189 PASS out of 205 runnable. Different database, so not a like-for-like
comparison — but every one of the seven regressions that run opened is PASS here.

## The four real failures

1. **`MultithreadedInsertionWithLazyConnectionTest`** — FAIL on all three arms
   (`found=2 ok=0 failed=2`), PASS on HotSpot. Also the 2026-08-20 Postgres
   run's only cross-GC HANG. The longest-standing item in this suite and the
   obvious next target.
2. **`techempower.TechEmpowerTest`** — FAIL on G1 and Generational, **PASS on
   ZGC**, PASS on HotSpot. Previously filed as the lambda-dispatch-timeout
   family; the collector split is new information and worth a repeat before
   anything is concluded from it.
3. **`MultithreadedIdentityGenerationTest`** — Generational only. It was also
   Generational-only in the 2026-08-20 Postgres run, so this is consistent
   rather than new.
4. **`SoftDeleteCollectionTest`** — Generational only, `found=4 ok=3 failed=1`.
   Single observation, not repeated.

Items 2-4 are single observations at this concurrency and have not had the
repeat-3-to-5x treatment this project's own discipline asks for before filing.

## What the fix cleared

Every class in the §8 deopt-resume family PASSes on **all three collectors** on
MySQL: `FilterWithPaginationTest`, `CriteriaMutationQueryTest`, `OneToManyTest`,
`RowIdUpdateAndDeleteTest`, `ReactiveStatelessWithBatchTest`,
`QuerySpecificationTest`, `MutationDelegateIdentityTest`, and the
`MutationDelegate*` / `SoftDelete*` / `EmbeddedId*` classes that pass 1's
saturated Docker had knocked out.

## Method note

Pass 1's numbers would have supported a much worse conclusion than the truth —
25 "failures" on ZGC including the fix's own witness class. The thing that
separated them was not re-reading logs but **re-running the non-PASS union at
the same concurrency as the control**. When a suite's setup depends on a shared
external resource (here, the Docker daemon), fork-count is an experimental
variable, and an arm run at 9-way against a control run at 2-way is not an A/B.

Raw per-class logs and `results.tsv`:
`runs/mysql-{default,g1,generational}-20260822-3gc-mysql/` (pass 1),
`runs/mysql-union-{default,g1,generational}/` and `runs/mysql-hotspot-union/` (pass 2).
