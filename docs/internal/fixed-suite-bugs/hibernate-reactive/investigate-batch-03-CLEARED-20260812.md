# hibernate-reactive investigate batch 03 — CLEARED, all 12 classes

**Status:** CLEARED (2026-08-12). All 12 classes pass on current `dev`, on the
collector each failure was recorded under, with the same test counts stock
HotSpot discovers, and — unlike batch 04 — with a **clean WARN census**: no
CratonVM diagnostic fires anywhere in the green run.

The page's failures were the two dominant blockers, now confirmed per-class
rather than assumed.

## Why they were failing

Both fixed 2026-08-12, both on `dev` before this check:

* `../vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md` —
  `javax/crypto/Mac.doFinal([BI)V` unregistered, so SCRAM's PBKDF2 died on
  iteration 2 of 4096 and every DB-required class lost its session.
* `../jna-native-clinit-nativeversion-npe-20260812-FIXED.md` — JNI `FindClass`
  could not return `java.lang.Object`, breaking Testcontainers' Docker probe.

## Verification

`dev` `33c0c1bdb`, binary built from that commit, Testcontainers `postgres:18.4`,
`TESTCONTAINERS_RYUK_DISABLED=true`. Every class was recorded `g1=FAIL`.

| class | tests | CratonVM G1 | HotSpot | result |
|---|---:|---:|---:|---|
| `EagerElementCollectionForEmbeddableEntityTypeMapTest` | 12 | 13 216 ms | 5 266 ms | PASS |
| `EagerElementCollectionForEmbeddableTypeListTest` | 31 | 19 400 | 5 583 | PASS |
| `EagerElementCollectionForEmbeddedEmbeddableMapTest` | 12 | 12 301 | 5 120 | PASS |
| `EagerElementCollectionForEmbeddedEmbeddableTest` | 31 | 18 762 | 5 342 | PASS |
| `EagerManyToOneAssociationTest` | 3 | 10 114 | 5 110 | PASS |
| `EagerOneToManyAssociationTest` | 2 | 9 270 | 5 727 | PASS |
| `EagerOneToOneAssociationTest` | 2 | 11 423 | 5 440 | PASS |
| `EagerOrderedElementCollectionForEmbeddableTypeListTest` | 31 | 16 550 | 6 783 | PASS |
| `EagerTest` | 4 | 13 906 | 6 351 | PASS |
| `EagerUniqueKeyTest` | 4 | 9 805 | 5 143 | PASS |
| `EmbeddedIdTest` | 7 | 13 839 | 5 373 | PASS |
| `EmbeddedIdWithManyEagerTest` | 3 | 8 967 | 5 244 | PASS |

**142 tests, 142 green**, on every configuration run:

| configuration | result |
|---|---|
| ZGC (the current default since 2026-08-10), 4 shards | 12/12 PASS |
| G1 (`-XX:+UseG1GC`), 4 shards | 12/12 PASS |
| G1, 6 shards, three repeat rounds | 12/12 PASS each |
| stock HotSpot JDK 25, same classpath | 12/12 PASS |

The three standing checks, same as batches 04 and 05:

* **The G1 flag reached the VM** — positive control: the same per-class override
  mechanism with `-XX:+UseSerialGC` produced the VM's *"unsupported garbage
  collector"* warning in the raw log. Without this, "12/12 on G1" rests on the
  flag not being silently dropped between the override table and `argv`.
* **CratonVM ran the same tests** — `found`/`ok` match HotSpot exactly per
  class, including the three 31-test collection classes.
* **Repeat rounds** — no class on this page was recorded HANG, but the batch was
  re-run three times at 6 shards anyway (36 further class-runs, all green),
  since every one of the twelve was a `FAIL` under concurrency.

## WARN census of the green run

Carrying batch 04's lesson — a passing suite is not a quiet VM — the whole G1
run's logs were counted, with the instrument controlled first:

| pattern | count |
|---|---:|
| CONTROL `@@RESULT` | 12 (expected 12) |
| CONTROL `WARN` | 290 (expected > 0) |
| `Stale pointer detected` | **0** |
| `timed out after` / `has been blocked for` | 0 / 0 |
| `code buffer estimate too small` / `codegen invariant cannot be encoded` | 0 / 0 |
| `SIGSEGV` / `panicked` | 0 / 0 |
| `NullPointerException` / `AbstractMethodError` / `NoSuchMethodError` / `ClassCastException` | 0 |
| `UnsupportedOperation` / `not implemented` / `unsupported` / `swallow` | 0 |

All 290 WARN lines are accounted for, and none is a defect:

* **204** `cratonvm::gc::guard` — the AUTOBOX_CLASS_ID boxing notice (W7-84),
  present in every CratonVM run on every workload.
* **84** `cratonvm_vm::vm::vm_util` — "Post-clinit fixup" progress notices.
* **2** `WARN [org.hibernate.orm.core] HHH000038: Composite id class does not
  override equals(): …EmbeddedIdWithManyEagerTest$Seed` — Hibernate's own
  application-level warning about a test fixture, not CratonVM.

That 204 + 84 + 2 = 290 is the point of writing them down: the two unexplained
lines were chased before this page was closed, exactly the arithmetic that
turned batch 04's twelve into a real defect. Here they were Hibernate's.

**`Stale pointer detected` = 0 is itself a result.** Batches 04 and 05 emitted
exactly one per test class; this run is on a `dev` that includes the fix for it
(`../stale-invokevirtual-receiver-false-positive-on-bare-object-20260812-FIXED.md`),
and the count is now zero on the same workload shape.

## Residual

CratonVM is **1.6x-3.5x** HotSpot's wall time per class here. Ordinary ratio for
a DB-backed reactive workload, nowhere near the 120 s `before()` budget, not a
defect.
