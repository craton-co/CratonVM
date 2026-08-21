# hibernate-orm JSON/XML function tests SIGSEGV under G1/ZGC — NOT REPRODUCIBLE, and the code hypothesis is refuted

**Status: NOT REPRODUCIBLE (2026-08-21). The 7 classes pass on both G1 and ZGC
on the exact commit the crashing runs used, against the same host and the same
harness. The crash was real — 14 observations across two runs — but its trigger
is environmental and is no longer present. No VM defect has been demonstrated,
and the "moving-collector pointer safety" reading in the original write-up is
contradicted by the code history.**

The original triage is preserved below the line. What follows is what was
actually measured when it was picked up.

## The decisive result

`509710ba8` is the `dev` HEAD both crashing runs used. Rebuilt it from source
(`md5 9ee1303259b0df1904ce9d74660c2239`) and re-ran all 7 classes through the
same `run-hib.sh` argfile and wrappers, against the same live Postgres:

| binary | G1 | ZGC |
| --- | --- | --- |
| `509710ba8` — **the crashing runs' own commit** | 7/7 pass | 7/7 pass |
| current `dev` `5b606e85e` | 7/7 pass | 7/7 pass |

`ok=N failed=0` on every one, and **`found`/`ok` match the HotSpot control
class-for-class** (3, 5, 4, 5, 3, 34, 8), so these are real passes and not a
suite that quietly ran fewer tests.

Same commit, same host, same harness, same database server, opposite outcome.
Whatever produced the crash was not in the VM revision.

## What that rules out

**The code.** Rebuilding the crashing runs' own commit is the control the
original page never ran. It passes. So the range `509710ba8..5b606e85e` did not
"fix" anything here, and no bisection of that range is worth doing.

**The "moving-collector pointer safety" inference.** The original page reasoned
from the always-G1-or-ZGC-never-Generational pattern to a relocation hazard.
`git diff --stat 509710ba8..5b606e85e -- gc/src` is **empty** — not one line of
collector code changed across the range. The inference was never supported by
anything but the pattern, and the pattern is now unreproducible.

**The JIT.** `--nojit` passes. So do both of the range's new switches
(`CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0`, `CRATONVM_JIT_LOCAL_HANDLERS=0`) —
neither brings the crash back, on either binary.

**The harness invocation.** Reproduced through the literal
`cratonvm-g1-wrapper.sh` / `cratonvm-zgc-wrapper.sh` (`-XX:+UseG1GC`, which
normalizes to the same collector as `--XX:UseGc=G1`), not a hand-built command.
Passes either way.

**A database that isn't there.** Pointing `hibernate.connection.url` at a
non-existent database yields `found=3 started=0 ok=0 failed=0` and `rc=0` on all
three collectors *and on HotSpot* — a clean skip, no crash. An unreachable DB is
not the trigger.

**HotSpot** (the original page's step 4, never done): passes all of them, same
counts. So there was never a HotSpot-vs-CratonVM divergence recorded for these.

## What changed, and the standing hypothesis

The Postgres container was restarted **after** both crashing runs and before
these:

```
ZGC rerun finished    2026-08-20 22:02
G1  rerun finished    2026-08-20 22:53
postgres StartedAt    2026-08-21 00:29:59Z   (RestartCount=0 — stopped and started by hand)
```

Both crashing runs therefore ran against a Postgres instance that had just
absorbed a 4548-class, 6-way-concurrent full suite (245 minutes) and was never
restarted in between. The isolated rerun's own log carries 68
`PSQLException`/`SQLException`/`FATAL`-class lines.

So the standing hypothesis is **a degraded database server state, not
concurrency of the client**. That is consistent with the original page's finding
that removing client-side concurrency changed nothing — the second run was
isolated, but it pointed at the *same un-restarted server*, so it did not
actually vary the thing that mattered. "Full isolation rules out contention" was
the wrong conclusion from a run that held the real variable fixed.

Note this does **not** excuse the VM: a SIGSEGV is never an acceptable response
to a misbehaving database. If the trigger can be recreated, there is very likely
a real defect on that error path. It simply has not been demonstrated yet, and
it is not where the original page pointed.

## How to re-catch it

Do not re-run the 7 classes in isolation — that has now been done eight
different ways and always passes. Reproduce the *server* condition:

1. Run the full 4548-class suite 6-way concurrent against a fresh Postgres, as
   `full-pg-*-20260820-3gc-pg-v2` did.
2. **Without restarting Postgres**, immediately re-run just these 7.
3. If they crash, capture `CRATONVM_SYMBOLIZE=<RVA>` against that exact binary
   *before* touching the container — the symbolized frame is the whole ask, and
   it is unobtainable once the server is restarted.
4. Record `docker inspect -f '{{.State.StartedAt}}'` in the run log so a future
   reader can tell whether the server was recycled between arms.

The harness should record the Postgres start time per run; without it, two runs
that look identical can differ in the one variable that decides the outcome.

---

# Original triage (2026-08-20), preserved

**Status at the time:** OPEN. Reproduced twice, independently, with 100%
overlap: the 6-way-concurrent full-suite run and a fully isolated 1-shard rerun
both crash the exact same 7 classes on both G1 and ZGC, zero on Generational.

## The 7 classes, identical on both runs

```
org.hibernate.orm.test.function.json.JsonExistsTest
org.hibernate.orm.test.function.json.JsonQueryTest
org.hibernate.orm.test.function.json.JsonTableTest
org.hibernate.orm.test.function.json.JsonValueTest
org.hibernate.orm.test.function.xml.XmlTableTest
org.hibernate.orm.test.query.hql.JsonFunctionTests
org.hibernate.orm.test.query.hql.XmlFunctionTests
```

`results.tsv` rows are `found=0 ok=0 failed=0 aborted=0 skipped=0` for all of
them. (Note for future readers: those zeros are what the harness writes when
there is **no** `@@RESULT` line at all, so they say nothing about how many tests
were discovered — they are not evidence that the crash preceded discovery.)

**Run 1** — full 4548-class suite, 3 GCs, 6-way concurrent shards, live
Postgres, `dev` HEAD `509710ba8`:

| GC | CRASH count | classes |
|---|---:|---|
| ZGC (default) | 8 | the 7 above + `schemaupdate.MySQLLobSchemaCreationTest` (one-off) |
| G1 | 7 | exactly the 7 above |
| Generational | 0 | — |

**Run 2** — same host, same HEAD, isolated rerun, 1 shard, no concurrency:

| GC | CRASH | wall |
|---|---:|---:|
| G1 | 7/31 (exactly the 7) | 83m20s |
| ZGC | 7/31 (exactly the 7) | 32m32s |

## Crash signature (`hs_err_pid3512.log`, G1 arm)

```
EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF633CD9D12
Faulting access: read at address 0x00007FF9E1B84694
gc collector: g1
jit: faulting pc not attributed to a compiled method
```

Captured Java frames were all JUnit Platform / Jupiter engine bootstrapping. The
faulting PC was not attributed to a compiled method and the native stack was
offsets-only — no symbolized stack was ever captured, which is why this page
could not be closed on its own evidence.
