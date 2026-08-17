# hibernate-reactive investigate batch 06 — CLEARED, all 11 classes

**Status:** CLEARED (2026-08-12). All 11 classes pass on current `dev`, on the
collector each failure was recorded under, with test outcomes matching stock
HotSpot **exactly — including a skip** — and a WARN census whose every line is
accounted for.

## Why they were failing

Both fixed 2026-08-12, both on `dev` before this check:

* `../vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md` —
  `javax/crypto/Mac.doFinal([BI)V` unregistered, so SCRAM's PBKDF2 died on
  iteration 2 of 4096 and every DB-required class lost its session.
* `../jna-native-clinit-nativeversion-npe-20260812-FIXED.md` — JNI `FindClass`
  could not return `java.lang.Object`, breaking Testcontainers' Docker probe.

## Verification

`dev` `e48ebe9d0`, binary built from that commit, Testcontainers `postgres:18.4`,
`TESTCONTAINERS_RYUK_DISABLED=true`.

| class | recorded | tests | CratonVM G1 | HotSpot | result |
|---|---|---:|---:|---:|---|
| `IdentityGeneratorWithColumnTransformerTest` | g1=FAIL | 1 | 22 240 ms | 10 823 ms | PASS |
| `ImplicitSoftDeleteTests` | g1=FAIL | 9 | 24 120 | 13 968 | PASS |
| `InternalStateAssertionsTest` | g1=FAIL | 4 | 22 959 | 11 170 | PASS |
| `JoinedInheritanceBatchTest` | g1=FAIL | 1 | 21 273 | 12 459 | PASS |
| `JoinedSubclassIdentityTest` | g1=FAIL | 2 | 14 354 | 10 226 | PASS |
| `JoinedSubclassInheritanceTest` | g1=FAIL | 8 (7 run, **1 skipped**) | 17 775 | 9 956 | PASS |
| `LazyInitializationExceptionTest` | g1=FAIL | 10 | 15 946 | 10 161 | PASS |
| `LazyManyToOneAssociationTest` | g1=HANG | 8 | 17 480 | 8 310 | PASS |
| `LazyOneToManyAssociationWithFetchTest` | g1=HANG | 8 | 13 983 | 9 901 | PASS |
| `LazyOneToOneWithJoinColumnTest` | g1=FAIL | 2 | 11 258 | 8 809 | PASS |
| `LazyOrderedElementCollectionForEmbeddableTypeListTest` | g1=FAIL | 2 | 11 754 | 9 300 | PASS |

**55 tests found, 54 green, 1 skipped**, on every configuration run:

| configuration | result |
|---|---|
| ZGC (the current default since 2026-08-10), 4 shards | 11/11 PASS |
| G1 (`-XX:+UseG1GC`), 4 shards | 11/11 PASS |
| G1, 6 shards, three repeat rounds | 11/11 PASS each |
| stock HotSpot JDK 25, same classpath | 11/11 PASS |

### The skip is the test's own, not a CratonVM gap

`JoinedSubclassInheritanceTest` reports `found=8 started=7 ok=7 skipped=1` on
CratonVM — the one number on this page that could mean "passed by not running
something". Stock HotSpot reports **`found=8 started=7 ok=7 skipped=1`**, the
same line. The skip is a JUnit assumption/`@Disabled` in the test itself.

This is exactly why the HotSpot comparison is `found`/`started`/`ok`/`skipped`
per class and not just a PASS verdict: a VM that quietly declines a test still
prints PASS.

**The G1 flag reached the VM** — `-XX:+UseG1GC` is accepted silently while
`-XX:+UseSerialGC` through the same per-class override mechanism produces the
VM's *"unsupported garbage collector"* warning, so silence is acceptance rather
than the flag being dropped between the override table and `argv`.

## WARN census of the green run

Instrument controlled first, per the batch-04 lesson:

| pattern | count |
|---|---:|
| CONTROL `@@RESULT` | 11 (expected 11) |
| CONTROL `WARN` | 265 (expected > 0) |
| `Stale pointer detected` | **0** |
| `timed out after` / `has been blocked for` | 0 / 0 |
| `code buffer estimate too small` / `codegen invariant cannot be encoded` | 0 / 0 |
| `SIGSEGV` / `panicked` | 0 / 0 |
| `NullPointerException` / `AbstractMethodError` / `NoSuchMethodError` / `ClassCastException` | 0 |
| `UnsupportedOperation` / `not implemented` / `unsupported` / `swallow` | 0 |

All 265 lines accounted for, none a defect: **187** `cratonvm::gc::guard`
(AUTOBOX_CLASS_ID notice), **77** `cratonvm_vm::vm::vm_util` (post-clinit
fixups), and **1** from Hibernate itself — `HHH90000033: Encountered use of
deprecated annotation [jakarta.persistence.Temporal] at
…JoinedSubclassInheritanceTest$Book.publish`. 187 + 77 + 1 = 265 exactly.

## Residual

CratonVM is **1.7x-2.1x** HotSpot's wall time per class here, measured on the
G1 4-shard arm. The repeat rounds drifted upward (`sum_class_ms` 193 k → 432 k
across three rounds) purely with host load — the box was carrying a load average
of ~16 by then — which is why the per-class table quotes one arm rather than a
mean, and why the verdict rests on PASS counts rather than timings. Nothing here
approaches the 120 s `before()` budget.
