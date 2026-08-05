# 16 synthetic-JDK class-library gaps, pinned as the interpreter corpus baseline

| | |
|---|---|
| **Status** | OPEN — measured, pinned, not fixed. Each is a gap in CratonVM's own ~5,200-stub class library, not an interpreter defect. |
| **Area** | `native-builtins` / `native-collections` (the `synthetic-jdk` class library), surfaced by `vm/tests/interpreter_tests.rs` |
| **Pinned in** | `KNOWN_SYNTHETIC_JDK_GAPS`, `vm/tests/interpreter_tests.rs` |
| **Discovered** | 2026-08-02, when the extended interpreter corpus was made runnable (see the retired `extended-interpreter-corpus-is-a-synthetic-jdk-corpus-FIXED-20260805` write-up for how it had been dark, and for the 198 failures that turned out not to be gaps at all). |

## Scope, and what "measured" means here

These are the residue of the `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1`
corpus after its harness was fixed: 924 tests, of which 908 produce the
JDK-correct answer and these 16 do not.

Every one was re-run under a **real JDK 25** — `java`, not `cratonvm` — and
HotSpot produced the value the test expects. So the expectation is right and
CratonVM's synthetic library is what is missing. (The corpus also contained
three expectations HotSpot *disagreed* with; those were corrected in the
fixtures rather than listed here.)

They matter as a compatibility signal, not as a product risk: the shipping
`cratonvm` CLI defaults to real-JDK mode and does not compile the synthetic
library in at all (`SYNTHETIC_JDK_COMPILED_IN`).

## The gaps

### Reflection — 6

| fixture · method | observed |
|---|---|
| `ReflectionComplete.testFieldGetOwnPrivateFinalReferenceWithoutSetAccessible` | `NullPointerException: Field.get: null receiver for instance field` |
| `ReflectionComplete.testProxyIsProxyClass` | returns 0 |
| `TckReflect.proxy_isProxyClass` | returns 0 |
| `TckReflect.ann_inheritedValue` | `ClassCastException: ? cannot be cast to cratonvm.TckReflect$TypeTag` |
| `TckReflect.ann_methodValue` | `ClassCastException: ? cannot be cast to cratonvm.TckReflect$TestInfo` |
| `TckReflect.proxy_objectMethods` | `ClassCastException: ? cannot be cast to cratonvm.TckReflect$Greeter` |

The three `ClassCastException`s are one shape: a synthetic stand-in is not
castable to the JDK interface it is meant to implement — the same family as the
`synthetic-standin-checkcast` rule (a synthetic proxy must implement the real
interface, not merely quack like it). `isProxyClass` returning 0 is the same
object seen from the other side.

`testFieldGetOwnPrivateFinalReferenceWithoutSetAccessible` is separate: a
declaring class reflecting on its own private-final field. `Field.get` rejects
the receiver as null. HikariCP's `HikariConfig.copyStateTo` uses exactly this
shape, so it is worth more than its one test.

### Generic reflection — 4

`GenericReflectionTest.testParameterizedField`, `.testParameterizedSuperclass`,
`.testTwoArgParameterizedField`, `.testWildcardExtendsNumber` — all return 0.
`getGenericType` / `getGenericSuperclass` do not surface `ParameterizedType` or
`WildcardType`.

### Serialization — 4

| fixture · method | observed |
|---|---|
| `SerializeBasic.testSimpleRoundTrip` | `NullPointerException: Cannot read field 'intValue' because the object is null` |
| `SerializeBasic.testNestedObject` | `NullPointerException: Cannot read field 'x' because the object is null` |
| `SerializeBasic.testTransientField` | `NullPointerException: Cannot read field 'transientValue' because the object is null` |
| `SerializeBasic.testNonSerializableThrows` | returns 0 — no `NotSerializableException` |

The first three are one bug: a round-trip through
`ObjectOutputStream`/`ObjectInputStream` returns an object whose fields all read
back null. Note that `serialization.rs` is behind
`experimental-serialization` / `synthetic-jdk`, so this is the synthetic
implementation, not the real-JDK path.

### Singles — 2

| fixture · method | observed |
|---|---|
| `FinalizerTest.testNoFinalizeOnLive` | returns 0 — `System.gc()` is not deterministic enough for the observation it asserts |
| `TckUtil.testArraysAsList` | returns 0 |

## Why they are pinned rather than ignored

`KNOWN_SYNTHETIC_JDK_GAPS` is a **two-way** gate. An unlisted mismatch fails
the run, and a listed pair that starts *passing* also fails the run, with a
message telling you to delete the entry. Closing a gap therefore always costs
one line of bookkeeping — which is the point: the predecessor corpus reported
a green `924 passed` while running none of itself, and a baseline whose entries
can close silently leaves the next regression with nothing to fail against.

## Reproducing

```bash
CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 cargo test --release -p cratonvm-vm --features synthetic-jdk --test interpreter_tests
```

To see a gap fail rather than be absorbed, delete its entry from
`KNOWN_SYNTHETIC_JDK_GAPS` first. The failure names the fixture, the method,
the expected value, and the thrown exception's class and detail message.
