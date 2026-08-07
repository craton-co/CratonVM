# Throughput-wall recurrence #3 — CLOSED: the moving-young fallback is not the cost, and nothing hangs

| | |
|---|---|
| **Status** | ✅ **CLOSED** — 2026-08-07. The HANG is not reproducible; the proposed mechanism is disproven quantitatively. The residual slowness is real and belongs to [`!webapp-deploy-annotation-scan-interpreted-226x.md`](../../../known-issues/tomcat/!webapp-deploy-annotation-scan-interpreted-226x.md), which now carries the measurements this investigation produced |
| **Original claim** | `TestHostConfigAutomaticDeployment*` (9 classes) and `TestNonBlockingAPI` HANG at the 1500 s ceiling standalone; persistent `[moving-young] fallback` → non-moving sweep is why |
| **Verdict** | HANG: **gone** (10/10 classes PASS). Mechanism: **wrong** — GC-side work is 0.4 % of the run. Magnitude: **real** (up to 32× HotSpot on the same host), and already owned elsewhere |
| **Discovered** | 2026-08-06 |
| **Settled** | 2026-08-07 on `dev` `e9c05391a`, Azure Linux host, real JDK 25 |

## 1. Nothing hangs

Every class the report named completes. Standalone, one JVM per class, with a
HotSpot control run back to back on the same host so the ratio is not a
statement about host load (which is why the absolute seconds vary — this box is
shared and ran between load 15 and 133 during the sweep):

| Class | CratonVM | HotSpot | ratio | load |
|---|---|---|---|---|
| `TestHostConfigAutomaticDeploymentAddition` | 404 s, OK (19) | 25 s | 16× | 76 |
| `…BrokenApp` | 6 s, OK (2) | 3 s | 2× | 118 |
| `…ContextClassName` | 21 s, OK (1) | 6 s | 3.5× | 122 |
| `…CopyXML` | 126 s, OK (8) | 10 s | 12× | 133 |
| `…DeleteA` | 77 s, OK (4) | 7 s | 10× | 127 |
| `…DeleteB` | 97 s, **13 failures** | 16 s, OK (11) | — | 121 |
| `…DeleteC` | 215 s, OK (9) | 13 s | 17× | 120 |
| `…Modification` | 448 s, OK (19) | 21 s | 21× | 47 |
| `…UpdateWarOffline` | 246 s, OK (8) | 13 s | 19× | 99 |
| `catalina.nonblocking.TestNonBlockingAPI` | 485 s, OK (44) | 55 s | 9× | 70 |

`DeleteB`'s failures are host contention, not a defect: re-run on the same
binary with the box at load ~15 it is **OK (11 tests)**, 178 s vs HotSpot 11 s.
Its first failure is a bare `AssertionError` and every later one is
`Unable to create appBase for test` / `this.tomcat is null` — the cascade of a
`@Before` that could not clean the previous test's appBase in time.

Cleanest single data point, box quiet (load 15), `Addition`:
**CratonVM 352 s / HotSpot 11 s = 32×**, both `OK (19 tests)`.

## 2. The fallback is not the cost

Same class, same binary, arms run back to back:

| Arm | Wall | minor GCs | moving cycles | fallbacks |
|---|---|---|---|---|
| default (moving-young requested) | 395 s | 101 | 7 | 69 |
| `CRATONVM_NO_MOVING_YOUNG=1` | 412 s | 101 | — | — |
| quiet box, default | 352 s | 101 | **0** | 77 |

Forcing the young generation to the non-moving sweep for *every* cycle costs
nothing measurable, and on the quiet run the copying collector never ran at all
(`cycles=0`) — yet the run is 352 s either way.

The A/B alone is not conclusive (the default arm is already 91 % non-moving, so
both arms are nearly the same configuration). The arithmetic is:

* **101 minor collections, 0 major, in 352–412 s.** For the young-collector mode
  to explain ~340 s of excess, each collection would have to cost ≈ 3.4 s.
* **Total card-refinement work across all 101 passes: 1.41 s** (`refinement_ms`
  from `CRATONVM_GC_STATS=1`). That is the measured GC-side cost: **0.4 %** of
  the run.
* Phase accounting (`--dump-phase-report`) attributes 394.7 s of a 395.0 s
  basis to `java_execution_ns`, with `gc_pause_ns=0`.

So the message the fallback prints — "the young generation is not actually a
copying collector" — is **true and irrelevant to this workload**: it allocates
4.3 M objects against 477 MB live and collects 101 times in six minutes. It is
not allocation-bound.

This is the same trap the ECJ `OperandStack` doc fell into, and for the same
reason: the fallback correlates with heavy JIT activity because it *is* the GC's
fail-closed response to it. See
[`ecj-operandstack-corruption-jsp-compilation-500s-FIXED.md`](ecj-operandstack-corruption-jsp-compilation-500s-FIXED.md).

## 3. Which obligations actually block moving-young

The report guessed at two conditions. Measured (`CRATONVM_GC_STATS=1`, `Addition`):

| Reason | default run | quiet run |
|---|---|---|
| `xt-helper-window-conservative-scan` | 57 | 70 |
| `innermost-rbp-belongs-to-unguarded-callee` | 7 | 6 |
| `compiled-frame-oop-not-published` | 4 | 0 |
| `unregistered-jit-frame-on-stack` | 1 | 1 |

Neither of the two the report named dominates. **83–91 % is
`XT_HELPER_WINDOW`** — a peer thread parked in a blocking syscall with JIT
frames below it on its native stack, scanned conservatively by
`xt_root_scan.rs` because its registers and raw stack cannot be rewritten.
Tomcat parks a pool of `http-nio-*-exec-N` workers exactly like that, so this
fires on essentially every collection of any embedded-server test.

That is a structural property of the cross-thread scan, not a regression from
the `interpreter.rs` split, and — per §2 — it is not costing this workload
anything. Making a blocked peer's JIT frames precisely rewritable is the
open cross-thread root-scan work (`BUG-03`, the `xt-takeover` line), not this
doc.

## 4. Where the residual really is

`--stack-sample-ms 50` over `CopyXML` (1516 samples ≈ 76 s of a 79 s run, i.e.
essentially all of it has an interpreted frame on top):

```
 55.21%  org/apache/tomcat/util/bcel/classfile/ConstantPool.getConstant
 12.47%  java/io/BufferedInputStream.fill
  5.74%  java/io/BufferedInputStream.read
  2.97%  org/apache/catalina/startup/ContextConfig.processAnnotationsJar
  2.64%  org/apache/tomcat/util/bcel/classfile/ConstantPool.<init>
  1.65%  org/apache/catalina/startup/ContextConfig.processResourceJARs
  1.52%  org/apache/catalina/startup/ContextConfig.processAnnotationsFile
```

`--dump-native-registry` over the same class: **48.2 M native invocations in
122 s**, and the shape is the BCEL class-file reader, not the GC:

```
12 645 096  java/io/DataInputStream.readByte()B
 6 935 427  java/util/Objects.requireNonNull(Object,String)
 6 934 818  java/io/DataInputStream.readUTF()
 6 523 648  java/io/DataInputStream.skipBytes(int)
 4 802 416  java/io/DataInputStream.readUnsignedShort()
 1 771 570  java/lang/Class.isAssignableFrom(Class)
 1 771 319  java/lang/Class.cast(Object)
 1 490 869  java/io/DataInputStream.readInt()
```

That is the webapp-deploy annotation scan, which already has an open doc.
The measurements above have been added to it; this doc is closed rather than
renamed because its subject — the moving-young fallback — is settled, and its
symptom is not.

## 5. One fix landed on the way

`ConstantPool.getConstant(int, Class)` runs `castTo.isAssignableFrom(…)` and
`castTo.cast(…)` once per constant-pool access, which is where the 1.77 M pairs
above come from. Both natives materialised class **names** before deciding:
`native_class_is_assignable_from` called `mirror_class_name` twice, and
`native_class_is_instance` called `class_name_of_id` twice just to compare
against fixed literals. Each of those takes the class-manager read lock and
clones the name into a fresh `String`.

Both now answer the ordinary "same class, or a subclass" shape from class ids
alone, before any name is materialised. The hoisted predicate is the first two
disjuncts of each function's own tail, and every branch in between returns `1`
or falls through — none returns `0` — so hoisting a `true` cannot change an
answer. `isInstance` excludes array targets, whose ids are not comparable.

Measured, 2 M iterations, arms interleaved A-B-B-A:

| | before | after | HotSpot |
|---|---|---|---|
| `Class.isAssignableFrom` | 793 ms | **565 ms** (1.40×) | 9 ms |
| `Class.isInstance` | 899 ms | **615 ms** (1.46×) | 7 ms |
| `Class.cast` (unchanged) | 452 ms | 442 ms | 10 ms |
| `checkcast` bytecode (unchanged) | 74 ms | 78 ms | 3 ms |

**On the deploy workload this is worth about 0.2 s of 122 s (0.17 %)** — 1.77 M
calls × 114 ns — and `CopyXML` measures the same before and after
(A-B-B-A: 116 / 111 / 116 / 122 s). It is kept because it is a strict
improvement to two JDK-surface methods used by every reflective framework, and
because it removes two lock acquisitions per call from a path several deploy
threads walk at once. It is **not** a fix for the residual, and the numbers
above are the reason to stop looking for the residual there.

## 6. Verification

* 10/10 named classes PASS standalone (table in §1), HotSpot controls run
  back to back.
* `cratonvm-native-builtins` 3315 passed / 0 failed, `cratonvm-jit` 1969 / 0,
  `cratonvm-vm` 2445 passed / 3 failed — the same 3 a pristine `origin/dev`
  worktree produces (`runtime_error_array_index_carries_index`,
  `no_unallowlisted_metadata_table_bypass_exists`,
  `the_allowlist_has_no_dead_rows`).
* Reproduction, for anyone re-checking:

```bash
cratonvm --java-home <jdk25> -Xmx2g -cp "$(cat .suite/cp-linux-fixed.txt)" \
  org.junit.runner.JUnitCore org.apache.catalina.startup.TestHostConfigAutomaticDeploymentAddition
```

To see the GC side, add `CRATONVM_GC_STATS=1` **and** drive JUnit through a
runner that does not call `System.exit` — `JUnitCore.main` does, which kills the
process before the shutdown summary prints, and that is why the original report
had only the rate-limited `fallback #N` warnings to go on and none of the
counters that contradict them.
