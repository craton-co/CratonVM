# 11 interpreter-corpus tests pass alone and fail in a full run

| | |
|---|---|
| **Status** | OPEN — measured and pinned, not fixed. A VM-lifecycle defect, not a class-library gap. |
| **Area** | per-VM teardown / residual process-global native state (`native-builtins`, `vm/src/vm/vm_init.rs`) |
| **Pinned in** | `KNOWN_ORDER_DEPENDENT`, `vm/tests/interpreter_tests.rs` |
| **Discovered** | 2026-08-05, verifying the corpus baseline by re-running each pinned entry with `--exact`. |

## The observation

Eleven corpus tests **pass when their test is the only one in the process** and
**fail once the ~900 other corpus tests have run first**. Each of those tests
stands up a `Vm`, uses it, and drops it. Nothing about the eleven changes; what
changes is how many VMs preceded them.

```bash
# passes
CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 \
  ./interpreter_tests --exact test_s19_parameterized_field
# fails, in the same binary
CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 ./interpreter_tests --test-threads=1
```

| fixture · method | failure in a full run |
|---|---|
| `ReflectionComplete.testFieldGetOwnPrivateFinalReferenceWithoutSetAccessible` | `NullPointerException: Field.get: null receiver for instance field` |
| `GenericReflectionTest.testParameterizedField` | returns 0 |
| `GenericReflectionTest.testParameterizedSuperclass` | returns 0 |
| `GenericReflectionTest.testTwoArgParameterizedField` | returns 0 |
| `GenericReflectionTest.testWildcardExtendsNumber` | returns 0 |
| `FinalizerTest.testNoFinalizeOnLive` | returns 0 |
| `TckUtil.testArraysAsList` | returns 0 |
| `SerializeBasic.testSimpleRoundTrip` | `NullPointerException: Cannot read field 'intValue' because the object is null` |
| `SerializeBasic.testNestedObject` | `NullPointerException: Cannot read field 'x' because the object is null` |
| `SerializeBasic.testTransientField` | `NullPointerException: Cannot read field 'transientValue' because the object is null` |
| `SerializeBasic.testNonSerializableThrows` | returns 0 — no `NotSerializableException` |

## Why this is the same family as the parallel crash

The retired `extended-interpreter-corpus-is-a-synthetic-jdk-corpus-FIXED-20260805`
write-up covers the fatal version of this: process-global native side-tables
keyed by something every VM mints from zero (a `ClassId`, a 32-bit identity
hash) handed VM N an object belonging to VM N−1. Eight such tables were moved
to `cratonvm_native_api::vm_scoped::VmScoped` and are now dropped by
`release_vm_native_state`, which is what stopped the SIGSEGV.

These eleven are the survivors: the same shape, but the stale row produces a
wrong *answer* instead of a wrong *address*, so nothing crashes. The generic
reflection and serialization clusters both go through per-class side-tables
that have not been audited yet; `testNoFinalizeOnLive` additionally depends on
`System.gc()` being decisive, which a heap already populated by hundreds of
prior VMs makes it not.

**Do not "fix" these by implementing the feature they appear to be missing** —
the feature works, as running the test alone proves. Find the table.

## Why they are pinned separately

`KNOWN_ORDER_DEPENDENT` absorbs the failure like `KNOWN_SYNTHETIC_JDK_GAPS`
does, but deliberately does NOT trip when an entry passes: whether it does
depends on execution order, so a parallel run legitimately flips some of them.
That makes it a weaker gate than the gap list, which is exactly why the two are
separate — an entry filed as a "class-library gap" would send the next reader
off to implement something that already works.

## Reproducing

```bash
CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 cargo test --release -p cratonvm-vm --features synthetic-jdk --test interpreter_tests -- --exact test_serialize_simple_round_trip
```

That passes. The same binary with no `--exact` fails it. The difference is the
process, not the code.
