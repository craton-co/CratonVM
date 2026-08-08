# 11 synthetic-JDK class-library gaps, pinned as the interpreter corpus baseline

| | |
|---|---|
| **Status** | OPEN — measured, pinned, not fixed. Genuine gaps in CratonVM's own ~5,200-stub class library, not interpreter defects. |
| **Area** | generic reflection, `java.io.Serializable` round-trips, `Field.get` on an own private-final field, `Arrays.asList` — all under the `synthetic-jdk` class library |
| **Pinned in** | `KNOWN_SYNTHETIC_JDK_GAPS`, `vm/tests/interpreter_tests.rs` |
| **Discovered** | 2026-08-02, when the extended interpreter corpus was made runnable. See the retired `extended-interpreter-corpus-is-a-synthetic-jdk-corpus-FIXED-20260805` write-up for why it had been dark, and for the 203 failures that turned out not to be gaps. |

## Scope, and what "measured" means here

These are what is left of the `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1`
corpus after its harness was fixed: 924 tests, 913 of which pass.

Each entry survived **two** checks, and both matter:

1. **Real JDK 25** (`probes/CorpusOracle`) produces the value the test expects,
   so the expectation is right and CratonVM is what is missing. Comparing
   CratonVM-synthetic against CratonVM-real cannot establish this — it says
   which of the two modes differs, not which one is correct, and three corpus
   expectations turned out to be wrong in exactly that blind spot.
2. **Run alone** (`--exact <test>`), in a process with no other VM before it —
   each still fails, so none of them is an artefact of the ~900 VMs that
   precede it in a full run.

They matter as a compatibility signal, not as a product risk: the shipping
`cratonvm` CLI defaults to real-JDK mode and does not compile the synthetic
library in at all (`SYNTHETIC_JDK_COMPILED_IN`).

> **Reading check 2 correctly.** `--exact` on a test that is already listed in
> `KNOWN_SYNTHETIC_JDK_GAPS` exits `FAILED` when the test *passes* — that is
> the list's unexpected-pass gate firing, not the fixture. Grepping for
> `result: FAILED` inverts the verdict for every listed entry. Read the panic
> text. This was got wrong once here, and it briefly produced a
> `corpus-is-order-dependent` write-up whose premise was exactly backwards.

## The gaps

| fixture · method | observed |
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

Three clusters. **Generic reflection** (4): `getGenericType` /
`getGenericSuperclass` never surface `ParameterizedType` or `WildcardType`.
**Serialization** (4): a round-trip through `ObjectOutputStream` /
`ObjectInputStream` returns an object whose fields all read back null, and
writing a non-`Serializable` raises nothing; note `serialization.rs` is behind
`experimental-serialization` / `synthetic-jdk`, so this is the synthetic
implementation, not the real-JDK path. **Singles** (3):
`Field.get` rejects the receiver as null when a declaring class reflects on its
own private-final field — the shape `HikariConfig.copyStateTo` uses, so worth
more than its one test; `Arrays.asList`; and a finalizer observation that needs
`System.gc()` to be more decisive than it is.

## Why they are pinned rather than ignored

`KNOWN_SYNTHETIC_JDK_GAPS` is a **two-way** gate. An unlisted mismatch fails
the run, and a listed pair that starts *passing* also fails the run, with a
message telling you to delete the entry. Closing a gap therefore always costs
one line of bookkeeping — which is the point: the predecessor corpus reported a
green `924 passed` while running none of itself, and a baseline whose entries
can close silently leaves the next regression with nothing to fail against.

That gate has already paid for itself once: five proxy/annotation tests were
listed here until it reported them passing, which is how they came off the list.

## Reproducing

```bash
CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 cargo test --release -p cratonvm-vm --features synthetic-jdk --test interpreter_tests
```

To see a gap fail on its own terms rather than be absorbed, delete its entry
first. The failure names the fixture, the method, the expected value, and the
thrown exception's class and detail message.
