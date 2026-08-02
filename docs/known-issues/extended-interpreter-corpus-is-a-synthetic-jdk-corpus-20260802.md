# The extended interpreter corpus measures the SYNTHETIC JDK, and 214 of its tests fail there

| | |
|---|---|
| **Status** | OPEN — triaged, not fixed. Needs a decision: quarantine, re-baseline, or fix the gaps. |
| **Area** | `vm/tests/interpreter_tests.rs` (the `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1` corpus) |
| **Symptom** | Opting in yields `710 passed; 214 failed` serially, and `STATUS_ACCESS_VIOLATION` in the default parallel run. |
| **Severity** | low as a product risk (it measures a non-default configuration), high as a *signal* risk (it read as green for as long as it was dark). |
| **Discovered** | 2026-08-02, after `f715d1367` fixed the build script that had been leaving the fixtures uncompiled. |

## Why it was invisible

Every corpus test is guarded twice:

```rust
if !require_extended_interpreter_tests(name) { return; }   // CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1
require_class_files!();                                     // $OUT_DIR/.../SimpleReturn.class exists
```

The second gate was always false. `vm/build.rs`'s legacy javac pass failed on
every build (one fixture needed a jar the build cannot supply), so no fixture
was staged and `SimpleReturn.class` never existed. **Opting in still produced a
green `924 passed` that had run none of the corpus.** Both gates now open.

## The corpus runs against the synthetic JDK, not the real one

`test_vm()` builds `VmConfig::new()`, i.e. `VmConfig::default()`, and

```rust
// vm/src/config.rs
pub const LAUNCHER_DEFAULT_JDK_MODE: JdkMode = JdkMode::Real;      // the cratonvm CLI
pub const EMBEDDED_DEFAULT_JDK_MODE:  JdkMode = JdkMode::Synthetic; // VmConfig::default()
```

This is deliberate and documented (`jdk-mode-determinism.md`): the in-tree
suite must stay hermetic rather than resolve JMODs from whatever JDK the build
machine has. The consequence is that this corpus exercises CratonVM's own
~5,200-stub class library — which is exactly what the module doc means by
"compatibility corpus probes rather than stable default CI gates".

**So most of these failures are synthetic-JDK class-library gaps, not
interpreter defects.** Measured, not assumed: every one of the 251 failing
`(class, method)` pairs was re-run through the real-JDK CLI with
`probes/CorpusMethodProbe.java`.

| | count |
|---|---:|
| failing under the synthetic JDK (the corpus) | 251 |
| …of which produce the EXPECTED value under the real-JDK CLI | **207** |
| …genuinely wrong under the real JDK too | 43 |
| …inconclusive (probe cannot call an instance method) | 1 |

The 43 that are wrong in both modes cluster hard:

| fixture | n | shape |
|---|---:|---|
| `ScopedValueComplete` | 20 | returns 0; `testBindNull` throws `NPE: Cannot store to object array because "cache" is null` |
| `TckJdbc` | 10 | every method returns 0 |
| `VirtualThreadTest` | 10 | `UnsatisfiedLinkError` on the fixture's OWN `native` declarations (`startVirtualThread`, `builderOfVirtualStart`, `threadIsVirtual`, …) |
| `PropertiesComplete.testPropertiesLoadSpaces` | 1 | returns 0 |
| `FinalizerTest.testNoFinalizeOnLive` | 1 | returns 0 |
| `ReflectionComplete.testFieldGetPrivate` | 1 | returns -1, want 42 |

## Already fixed here: 37 tests that were broken in every mode

`PgoTest.java` and `FPCompletenessTest.java` declared **no package** while
living in `vm/tests/resources/cratonvm/` and being invoked as
`cratonvm/PgoTest` / `cratonvm/FPCompletenessTest`. javac staged them in the
default package, so all 37 of their tests failed with `ClassNotFound`. The
committed `.class` files hid it: their `this_class` says `PgoTest` while their
path says `cratonvm/PgoTest` — a contradiction HotSpot rejects outright and
only CratonVM's loader tolerated. Adding `package cratonvm;` took the corpus
from 251 to **214** failures.

(Four more sources still declare no package — `JitDifferential`,
`JitSafepointStress`, `ManifestNullValue`, `ToolProviderProbe` — but no corpus
test invokes them under a package name. `JitDifferential` is launched as
`cratonvm.JitDifferential` from `jit_interp_differential.rs` against the
*committed* tree, so it works today and is left alone. Also noted:
`PgoTest$Rect/$Shape/$Square.class` are committed orphans; the current source
declares no such types.)

## The parallel run crashes

Without `--test-threads=1` the test binary dies with `STATUS_ACCESS_VIOLATION
(0xc0000005)` partway through — many in-process VMs running concurrently. It
does not reproduce serially. This is independent of any individual assertion
and is the more interesting of the two problems.

## The default suite is unaffected

With `CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS` unset the corpus stays off and
`cargo test -p cratonvm-vm --test interpreter_tests` is 924/924 green, before
and after. Nothing here is a CI regression.

## Recommendation

1. **Do not** switch `test_vm()` to real-JDK mode to make the numbers look
   better — the hermeticity that makes it synthetic is a declared invariant of
   the in-tree suite, and flipping it would silently change ~5,000 tests.
2. **Re-baseline**: record the 214 as the known synthetic-mode baseline so the
   corpus becomes runnable and *regressions* against it are visible. A corpus
   that is 83% "known synthetic gaps" is still worth having if the number is
   pinned.
3. **Then** triage the 43 real-JDK failures on their merits, worst cluster
   first (`ScopedValueComplete`, `TckJdbc`, `VirtualThreadTest`). Those are the
   only ones that say anything about the configuration the product ships.
4. Investigate the parallel crash separately; it is a VM-lifecycle question,
   not a corpus question.

## Reproducing

```bash
CRATONVM_RUN_EXTENDED_INTERPRETER_TESTS=1 \
  cargo test --release -p cratonvm-vm --test interpreter_tests -- --test-threads=1
```

To ask what a single fixture method does under the real JDK (the corpus only
reports `Err(ExceptionThrown(ObjectRef { .. }))`, an address with no class,
message or stack):

```bash
cratonvm -cp "<probe-dir>;<staged-classes>" CorpusMethodProbe cratonvm/ScopedValueComplete testCallReturn
```
