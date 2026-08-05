# 5 synthetic-JDK class-library gaps, pinned as the interpreter corpus baseline

| | |
|---|---|
| **Status** | OPEN — measured, pinned, not fixed. Genuine gaps in CratonVM's own ~5,200-stub class library, not interpreter defects. |
| **Area** | `java.lang.reflect.Proxy` + annotation proxies under the `synthetic-jdk` class library (`native-builtins/src/reflect_annotations.rs`, `lang_class.rs`) |
| **Pinned in** | `KNOWN_SYNTHETIC_JDK_GAPS`, `vm/tests/interpreter_tests.rs` |
| **Discovered** | 2026-08-02, when the extended interpreter corpus was made runnable. See the retired `extended-interpreter-corpus-is-a-synthetic-jdk-corpus-FIXED-20260805` write-up for why it had been dark, and for the 209 failures that turned out not to be gaps at all. |

## Scope, and what "measured" means here

These are what is left of the `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1`
corpus after its harness was fixed: 924 tests, of which 908 pass, 11 are
order-dependent (`corpus-is-order-dependent-20260805.md`), and these 5 are real.

Each survived **two** checks, and both matter:

1. **Real JDK 25** (`probes/CorpusOracle`) produces the value the test expects,
   so the expectation is right and CratonVM is what is missing. Comparing
   CratonVM-synthetic against CratonVM-real cannot establish this — it says
   which of the two modes differs, not which one is correct, and three corpus
   expectations turned out to be wrong in exactly that blind spot.
2. **Run alone** (`--exact <test>`), in a process with no other VM before it,
   each still fails. Eleven entries that passed this check are filed as order
   dependence instead; a "gap" that disappears when you run it by itself is not
   a gap.

They matter as a compatibility signal, not as a product risk: the shipping
`cratonvm` CLI defaults to real-JDK mode and does not compile the synthetic
library in at all (`SYNTHETIC_JDK_COMPILED_IN`).

## The gaps

| fixture · method | observed |
|---|---|
| `ReflectionComplete.testProxyIsProxyClass` | returns 0 |
| `TckReflect.proxy_isProxyClass` | returns 0 |
| `TckReflect.ann_inheritedValue` | `ClassCastException: ? cannot be cast to cratonvm.TckReflect$TypeTag` |
| `TckReflect.ann_methodValue` | `ClassCastException: ? cannot be cast to cratonvm.TckReflect$TestInfo` |
| `TckReflect.proxy_objectMethods` | `ClassCastException: ? cannot be cast to cratonvm.TckReflect$Greeter` |

All five are one shape seen from two sides: a generated proxy is not
recognisable *as* a proxy (`isProxyClass` → 0) and is not castable *to* the
interface it is supposed to implement. That is the standing "a synthetic
stand-in must implement the real JDK interface, not merely quack like it" rule
— a `checkcast` asks the class hierarchy, and the generated `$ProxyN` does not
satisfy it for these shapes. The annotation cases are the same defect reached
through `getAnnotation`, whose proxy must be castable to the annotation
interface.

The `?` in the `ClassCastException` message is `class_name_of_id` declining to
name the source class. Note that in a *parallel* run these five flip
intermittently; that is residue of the VM-scoping work described in the retired
write-up, not evidence that the gap is closing.

## Why they are pinned rather than ignored

`KNOWN_SYNTHETIC_JDK_GAPS` is a **two-way** gate. An unlisted mismatch fails
the run, and a listed pair that starts *passing* also fails the run, with a
message telling you to delete the entry. Closing a gap therefore always costs
one line of bookkeeping — which is the point: the predecessor corpus reported a
green `924 passed` while running none of itself, and a baseline whose entries
can close silently leaves the next regression with nothing to fail against.

## Reproducing

```bash
CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 cargo test --release -p cratonvm-vm --features synthetic-jdk --test interpreter_tests
```

To see a gap fail rather than be absorbed, delete its entry from
`KNOWN_SYNTHETIC_JDK_GAPS` first. The failure names the fixture, the method,
the expected value, and the thrown exception's class and detail message.
