# hibernate-reactive investigate batch 04 — CLEARED, all 12 classes (and one real bug found)

**Status:** CLEARED (2026-08-12). All 12 classes pass on current `dev`, on the
collector each failure was recorded under, with the same test counts stock
HotSpot discovers. The page's failures were the two dominant blockers, now
confirmed per-class rather than assumed — **but the investigation did turn up a
genuine VM defect** the passing tests were hiding: see
`../stale-invokevirtual-receiver-false-positive-on-bare-object-20260812-FIXED.md`.

## Why they were failing

Both fixed 2026-08-12, both on `dev` before this check:

* `../vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md` —
  `javax/crypto/Mac.doFinal([BI)V` unregistered, so SCRAM's PBKDF2 died on
  iteration 2 of 4096 and every DB-required class lost its session.
* `../jna-native-clinit-nativeversion-npe-20260812-FIXED.md` — JNI `FindClass`
  could not return `java.lang.Object`, breaking Testcontainers' Docker probe.

## Verification

`dev` `8763197f2`, binary built from that commit, Testcontainers `postgres:18.4`,
`TESTCONTAINERS_RYUK_DISABLED=true`.

| class | recorded | tests | CratonVM G1 | HotSpot | result |
|---|---|---:|---:|---:|---|
| `EmbeddedIdWithManyTest` | g1=FAIL | 2 | 14 384 ms | 6 649 ms | PASS |
| `EmbeddedIdWithOneToOneTest` | g1=FAIL | 1 | 11 946 | 5 965 | PASS |
| `ExternalTransactionTest` | g1=FAIL | 8 | 13 404 | 5 824 | PASS |
| `FetchModeSubselectEagerTest` | g1=FAIL | 3 | 17 568 | 5 438 | PASS |
| `FetchedAssociationTest` | g1=FAIL | 1 | 12 109 | 6 366 | PASS |
| `FilterTest` | g1=FAIL | 4 | 16 789 | 7 176 | PASS |
| `FilterWithPaginationTest` | g1=FAIL | 35 | 20 787 | 8 141 | PASS |
| `FindAfterFlushTest` | g1=HANG | 4 | 9 438 | 8 879 | PASS |
| `FindByIdWithLockTest` | g1=FAIL | 2 | 10 968 | 7 300 | PASS |
| `FormulaTest` | g1=FAIL | 1 | 10 736 | 7 152 | PASS |
| `GeneratedPropertyJoinedTableTest` | g1=HANG | 2 | 9 135 | 6 849 | PASS |
| `GeneratedPropertySingleTableTest` | g1=FAIL | 2 | 10 968 | 7 969 | PASS |

**65 tests, 65 green**, on every configuration run:

| configuration | result |
|---|---|
| ZGC (the current default since 2026-08-10), 4 shards | 12/12 PASS |
| G1 (`-XX:+UseG1GC`), 4 shards | 12/12 PASS |
| G1, 6 shards, three repeat rounds | 12/12 PASS each |
| stock HotSpot JDK 25, same classpath | 12/12 PASS |

Same three things not taken on trust as in batch 05:

* **That the G1 flag reached the VM.** No log line names the collector, so
  "12/12 on G1" would otherwise rest on the flag surviving the runner's
  per-class override table into `argv`. Positive control: the same mechanism
  with `-XX:+UseSerialGC` produced the VM's *"unsupported garbage collector"*
  warning in the raw log.
* **That CratonVM ran the same tests.** `found`/`ok` match HotSpot exactly per
  class, including the 35-test `FilterWithPaginationTest` and the five classes
  that genuinely declare only one or two.
* **That one green run disproves a HANG.** Two of the twelve were recorded HANG;
  re-run three more times at 6 shards, 36 further class-runs, all green.

## What a passing run was hiding

Every run — both collectors, both batches, five sweeps — carried **exactly one**
`Stale pointer detected in invokevirtual receiver (all-zero header)` per test
class. One-per-class across 8 runs is not a race, and it turned out to be a
false positive on a healthy `new Object()`: `class_id`, `shape` and `mark_word`
are all legitimately zero for a no-field `java.lang.Object`, so the detector
cannot distinguish one from zeroed GC memory. It reproduces in six lines of
Java with no Hibernate involved, and it had been fixed once before and silently
un-fixed by the 2026-08-06/07 header shrink. Fixed, with the full root cause,
in the record linked at the top.

The lesson for the remaining four pages: **census the WARN lines of a green run**
before closing it. The tests passing is not the same as the VM being quiet, and
the per-class regularity (12 classes → 12 warnings) is what made this one worth
pulling on.

## Residual

CratonVM is **1.1x-3.2x** HotSpot's wall time per class here. That is the
ordinary ratio on a DB-backed reactive workload, nowhere near the 120 s
`before()` budget, and not a defect.
