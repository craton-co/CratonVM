# hibernate-reactive investigate batch 02 — CLEARED, all 12 classes

**Status:** CLEARED (2026-08-12). All 12 classes pass on current `dev` on **all
four** collectors, with the same test counts stock HotSpot discovers and a WARN
census in which every line is accounted for.

This page needed more than batches 03/04/05: its classes were the only ones
recorded failing on **all three** GC variants at once, and ten of the twelve
carried a mixed `FAIL/HANG` verdict — the page's own header calls that "raw
observed flakiness near the run's stop point, not yet explained". A single green
arm would not have answered it.

## Why they were failing

Both fixed 2026-08-12, both on `dev` before this check:

* `../vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md` —
  `javax/crypto/Mac.doFinal([BI)V` unregistered, so SCRAM's PBKDF2 died on
  iteration 2 of 4096 and every DB-required class lost its session.
* `../jna-native-clinit-nativeversion-npe-20260812-FIXED.md` — JNI `FindClass`
  could not return `java.lang.Object`, breaking Testcontainers' Docker probe.

The mixed FAIL/HANG is consistent with that: a class whose session never opens
either errors out or sits until the wall clock kills it, depending on where the
shard was when the run was stopped.

## Verification

`dev` `747a7c433`, binary built from that commit, Testcontainers `postgres:18.4`,
`TESTCONTAINERS_RYUK_DISABLED=true`.

| class | recorded (default / g1 / zgc) | tests | CratonVM G1 | HotSpot | result |
|---|---|---:|---:|---:|---|
| `CollectionStatelessSessionListenerTest` | HANG / FAIL / FAIL | 4 | 10 269 ms | 5 150 ms | PASS |
| `CompositeIdTest` | FAIL / FAIL / FAIL | 5 | 11 128 | 5 591 | PASS |
| `CompositeIdWithGeneratedValuesTest` | HANG / FAIL / HANG | 1 | 9 020 | 4 969 | PASS |
| `CriteriaMutationQueryTest` | HANG / FAIL / FAIL | 9 | 12 718 | 5 104 | PASS |
| `CustomGeneratorTest` | HANG / FAIL / HANG | 1 | 10 877 | 5 414 | PASS |
| `CustomOneToOneStoredProcedureSqlTest` | HANG / FAIL / HANG | 4 | 13 278 | 6 626 | PASS |
| `CustomSqlTest` | HANG / FAIL / HANG | 1 | 10 392 | 5 903 | PASS |
| `CustomStoredProcedureSqlTest` | HANG / FAIL / FAIL | 5 | 13 659 | 6 540 | PASS |
| `DynamicUpdateTest` | FAIL / FAIL / HANG | 1 | 11 542 | 6 345 | PASS |
| `EagerElementCollectionForBasicTypeListTest` | FAIL / FAIL / HANG | 29 | 17 233 | 7 420 | PASS |
| `EagerElementCollectionForBasicTypeMapTest` | FAIL / FAIL / HANG | 12 | 14 967 | 5 222 | PASS |
| `EagerElementCollectionForBasicTypeSetTest` | — / FAIL / FAIL | 23 | 16 597 | 5 792 | PASS |

**95 tests, 95 green**, across seven sweeps — 84 class-runs, no failure of any
kind:

| configuration | shards | result |
|---|---|---|
| default (ZGC since 2026-08-10) | 4 | 12/12 PASS |
| G1 (`-XX:+UseG1GC`) | 4 | 12/12 PASS |
| ZGC (`-XX:+UseZGC`, explicit) | 4 | 12/12 PASS |
| Generational (`-XX:+UseGenerationalGC`) | 4 | 12/12 PASS |
| G1, repeat round 1 | 6 | 12/12 PASS |
| G1, repeat round 2 | 6 | 12/12 PASS |
| Generational, repeat round | 6 | 12/12 PASS |
| stock HotSpot JDK 25, same classpath | — | 12/12 PASS |

### The collector controls, done properly

For batches 03-05 one `-XX:+UseSerialGC` positive control was enough, because
only one variant was under test. Here three different collector names had to be
shown to be *distinguished*, not merely delivered:

| flag | "unsupported garbage collector" warning | reading |
|---|---|---|
| `-XX:+UseG1GC` | 0 | parsed and accepted |
| `-XX:+UseZGC` | 0 | parsed and accepted |
| `-XX:+UseGenerationalGC` | 0 | parsed and accepted |
| `-XX:+UseSerialGC` | **1** | parsed and *rejected* — the parser is being consulted |

The Serial arm is what makes the other three meaningful: it proves the flag
reaches `parse_gc_algorithm` and that silence for the other three is acceptance,
not the flag being dropped on the floor between the runner's per-class override
table and `argv`.

**CratonVM ran the same tests**: `found`/`ok` match HotSpot exactly per class,
including the three large collection classes (29, 23 and 12 tests).

## WARN census of the green run

Instrument controlled first, per the batch-04 lesson:

| pattern | count |
|---|---:|
| CONTROL `@@RESULT` | 12 (expected 12) |
| CONTROL `WARN` | 289 (expected > 0) |
| `Stale pointer detected` | **0** |
| `timed out after` / `has been blocked for` | 0 / 0 |
| `code buffer estimate too small` / `codegen invariant cannot be encoded` | 0 / 0 |
| `SIGSEGV` / `panicked` / `StackOverflow` / `OutOfMemory` | 0 |
| `NullPointerException` / `AbstractMethodError` / `NoSuchMethodError` / `ClassCastException` | 0 |
| `UnsupportedOperation` / `not implemented` / `swallow` | 0 |

All 289 lines accounted for, none a defect: **204** `cratonvm::gc::guard`
(AUTOBOX_CLASS_ID notice, present on every workload), **84**
`cratonvm_vm::vm::vm_util` (post-clinit fixups), and **1** `HHH000038` from
Hibernate itself about `CompositeIdWithGeneratedValuesTest$ProductId` not
overriding `equals()` — an application warning about a test fixture.

204 + 84 + 1 = 289 exactly. That arithmetic is the point: it is what turned
batch 04's twelve unexplained lines into a real VM defect.

## Residual

CratonVM is **1.8x-2.9x** HotSpot's wall time per class here — the tightest and
most uniform spread of the four batches cleared so far. Ordinary ratio for a
DB-backed reactive workload, nowhere near the 120 s `before()` budget, not a
defect.
