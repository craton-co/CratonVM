# hibernate-orm JSON/XML function tests SIGSEGV under G1/ZGC, never under Generational — OPEN

**Status:** OPEN. Reproduced twice, independently, with 100% overlap: the
6-way-concurrent full-suite run and a fully isolated 1-shard rerun both crash
the exact same 7 classes on both G1 and ZGC, zero on Generational. Root cause
not yet located — no symbolized native stack yet, no bisection to a specific
JSON/XML code path done.

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

Every one of the 7 is JSON- or XML-function-related — no other class in
either run showed this signature. `results.tsv` rows are `found=0 ok=0
failed=0 aborted=0 skipped=0` for all of them: the crash happens before any
`@Test` method runs, i.e. during class-level `@BeforeAll`/fixture setup, not
inside a specific function assertion.

## Evidence trail

**Run 1** — `apps/hib-suite-runner`, full 4548-class suite, 3 GCs, 6-way
concurrent shards, live Postgres, `dev` HEAD `509710ba8`
(`runs/full-pg-{zgc,g1,generational}-20260820-3gc-pg-v2`):

| GC | CRASH count | classes |
|---|---:|---|
| ZGC (default) | 8 | the 7 above + `schemaupdate.MySQLLobSchemaCreationTest` (one-off, not reproduced elsewhere, not part of this record) |
| G1 | 7 | exactly the 7 above |
| Generational | 0 | — |

**Run 2** — same host, same `dev` HEAD, **isolated rerun**: just these 7
classes (plus 24 other unrelated FAIL/HANG/CRASH survivors from run 1),
**1 shard, no concurrency**, fresh Postgres
(`runs/ofhc-{g1,zgc}-rerun-fhc-20260820`):

| GC | CRASH | wall |
|---|---:|---:|
| G1 | 7/31 (exactly the 7) | 83m20s |
| ZGC | 7/31 (exactly the 7) | 32m32s |

Full isolation (no concurrent shards, no Testcontainers/Docker contention,
fresh DB) does not change the outcome at all — rules out resource contention
as the cause, unlike most of the other FAIL/HANG noise in the same rerun
(see the companion `hib-reactive` doc's contention findings from the same
session for the contrast).

## Crash signature (`hs_err_pid3512.log`, G1 arm)

```
EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF633CD9D12
Faulting access: read at address 0x00007FF9E1B84694
gc collector: g1
jit: faulting pc not attributed to a compiled method
```

`rc=139` (128+SIGSEGV) on the harness side, matching. The captured Java
frames are all JUnit Platform / Jupiter engine bootstrapping
(`SessionPerRequestLauncher.execute` → ... → `ClassBasedTestDescriptor
.invokeBeforeAllMethods`) — consistent with the fault landing during
`@BeforeAll`/fixture setup, before any hibernate-orm JSON/XML code the test
itself exercises has necessarily run. The faulting PC is **not attributed to
a compiled method**, and the native stack is offsets-only (no symbols by
default). Every hs_err in this run shows the same shape; only one is quoted
here.

## What's NOT yet known

* No symbolized stack — `CRATONVM_SYMBOLIZE=<RVAs> cratonvm` was not run
  against this exact binary/crash yet.
* Not confirmed whether the fault is actually inside JSON/XML-specific
  native code, or something more generic that these 7 classes' fixture setup
  happens to trigger (e.g. a moving-GC unsafe-pointer window during class
  init that any sufficiently-shaped `@BeforeAll` could hit — the "always G1
  or ZGC, never Generational" pattern points at a moving-collector pointer
  safety issue, not a JSON/XML-domain bug per se, but that's an inference,
  not yet verified).
* Not checked against HotSpot (expected to pass — not yet confirmed for
  these specific 7 classes on this exact fixture).
* `MySQLLobSchemaCreationTest`'s one-off CRASH on ZGC-only (run 1, not
  reproduced in run 2) is NOT part of this record — different class, only
  seen once, not investigated.

## Next steps

1. Get a symbolized native stack: re-run one of the 7 (e.g. `JsonExistsTest`
   in isolation) under G1 with `CRATONVM_SYMBOLIZE` against the exact crash
   RVA, or attach a debugger/`CRATONVM_DBG_JIT_NAMES=1` to catch it live.
2. Since the fault lands in `@BeforeAll`, check what these 7 classes'
   fixtures share that the rest of the suite doesn't — likely a JSON/XML
   Postgres type mapping or native codec registered once per class, touched
   during schema/session setup rather than during an actual test body.
3. Confirm the "moving collector only" read as a GC-safety bug (a raw
   pointer held across a safepoint that G1/ZGC can relocate through, that
   Generational's current non-moving-in-this-config behavior happens not to
   disturb) rather than assuming it from the collector pattern alone.
4. Verify against HotSpot on the identical classpath before spending further
   time — not yet done.
