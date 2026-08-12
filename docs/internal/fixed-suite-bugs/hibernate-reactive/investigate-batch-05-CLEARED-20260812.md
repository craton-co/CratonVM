# hibernate-reactive investigate batch 05 — CLEARED, all 12 classes

**Status:** CLEARED (2026-08-12). Every class on this page passes on current
`dev`, on the collector its failure was recorded under, with the same test
counts stock HotSpot discovers. No distinct defect was hiding behind the two
dominant blockers.

The page was one of six built from a **partial** 3-GC-variant PostgreSQL run on
Azure host `azureuser@20.80.105.49` that was stopped early once a dominant
blocker was identified. Its own header said the classes were *"LIKELY hitting
that same SASL/SCRAM handshake bug … but that has NOT been confirmed per-class"*.
This record is that per-class confirmation for batch 05.

## Why they were failing

Two blockers, both root-caused and fixed on 2026-08-12, both on `dev` well
before this check:

* `docs/internal/fixed-suite-bugs/vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md`
  — `javax/crypto/Mac.doFinal([BI)V` was never registered, so SCRAM's PBKDF2
  died on iteration 2 of 4096 and every DB-required class lost its session.
* `docs/internal/fixed-suite-bugs/jna-native-clinit-nativeversion-npe-20260812-FIXED.md`
  — JNI `FindClass` could not return `java.lang.Object`, breaking Testcontainers'
  rootless-Docker probe.

## Verification

`dev` `dba92735b`, binary built from that commit, Testcontainers `postgres:18.4`,
`TESTCONTAINERS_RYUK_DISABLED=true`.

| class | recorded | tests | CratonVM G1 | HotSpot | result |
|---|---|---:|---:|---:|---|
| `GeneratedPropertyUnionSubclassesTest` | g1=FAIL | 1 | 7 646 ms | 3 302 ms | PASS |
| `HQLQueryParameterNamedLimitTest` | g1=FAIL | 10 | 9 835 | 4 064 | PASS |
| `HQLQueryParameterNamedTest` | g1=FAIL | 10 | 12 402 | 3 695 | PASS |
| `HQLQueryParameterPositionalLimitTest` | g1=FAIL | 9 | 9 901 | 3 740 | PASS |
| `HQLQueryParameterPositionalTest` | g1=HANG | 10 | 12 383 | 3 809 | PASS |
| `HQLQueryTest` | g1=HANG | 9 | 17 425 | 3 742 | PASS |
| `HQLUpdateQueryTest` | g1=FAIL | 4 | 10 385 | 3 450 | PASS |
| `IdentifierGenerationTypeTest` | g1=FAIL | 6 | 8 347 | 3 558 | PASS |
| `IdentityGenerationWithBatchingTest` | g1=FAIL | 1 | 6 934 | 3 387 | PASS |
| `IdentityGeneratorDynamicInsertTest` | g1=FAIL | 1 | 7 414 | 3 486 | PASS |
| `IdentityGeneratorTest` | g1=FAIL | 1 | 8 306 | 3 417 | PASS |
| `IdentityGeneratorTypeTest` | g1=HANG | 2 | 7 147 | 3 296 | PASS |

**64 tests, 64 green**, on every configuration run:

| configuration | result |
|---|---|
| ZGC (the current default since 2026-08-10), 4 shards | 12/12 PASS |
| G1 (`-XX:+UseG1GC`), 4 shards | 12/12 PASS |
| G1, 6 shards, three repeat rounds | 12/12 PASS each |
| stock HotSpot JDK 25, same classpath | 12/12 PASS |

### Three things this check did NOT take on trust

* **That the G1 flag reached the VM.** No log line names the collector, so
  "12/12 on G1" would otherwise have rested on the flag not being silently
  dropped between the runner's per-class override table and `argv` — the
  `--add-opens`-was-parsed-then-ignored failure mode. Positive control: the same
  override mechanism with `-XX:+UseSerialGC` produced the VM's
  *"Warning: unsupported garbage collector"* in the raw log, proving the path
  end to end. (The variant matters here: the failures were recorded on G1, and
  since 2026-08-10 the *default* is ZGC, not Generational — so "default" and
  "g1" are two different collectors and neither is the one the original run
  called default.)
* **That CratonVM was running the same tests.** A VM that silently discovers
  fewer tests passes for the wrong reason. `found`/`ok` were compared per class
  against stock HotSpot and match exactly, including the four classes that
  genuinely declare only one or two tests.
* **That one green run disproves a HANG.** Three of the twelve were recorded as
  HANG, which is a timing verdict. Re-run three more times at 6 shards (closer
  to the original run's concurrency than the 4 used for the first pass) — 36
  further class-runs, all green.

## Residual

CratonVM is **2.2x-4.7x** HotSpot's wall time per class here (e.g. `HQLQueryTest`
17 425 ms vs 3 742 ms). That is the ordinary CratonVM/HotSpot ratio on a
DB-backed reactive workload, not a defect, and none of it approaches the 120 s
`before()` budget these classes are run under. Recorded so the next person
reading this page does not re-open it as a performance bug.
