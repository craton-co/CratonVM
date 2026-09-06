# 19 netty classes fail on **Generational only** — the moving young cycle corrupts a live object

**Status:** OPEN. Found 2026-09-06 by a per-collector sweep of the full netty
suite. **This is a correctness defect, not a throughput one**, and it is
invisible to every run that uses the shipped default collector.

| | |
|---|---|
| **Severity** | Medium-high. Wrong answers (NPE on a live object), CratonVM-only, Generational-only, intermittent but reproducible 3/3 standalone. |
| **Reachable how** | `-XX:+UseGenerationalGC` on multi-threaded code. ZGC is the shipped default, so nobody hits it by accident — which is exactly why it sat unseen. |
| **Binary** | `cvm-netty3gc-20260906.exe`, dev `8d83c7585`. |

---

## 1. The measurement that found it

The full netty suite (657 classes) run **once per collector, from one binary**,
4 shards each. ZGC is in the run as the same-binary control: the recorded
baseline (`netty-nonpassed-latest.txt`) is a default-collector run on a dev tip
eight days older, so a Generational-vs-baseline diff would have conflated the
collector with eight days of dev movement.

524 classes reported on all three arms and are therefore comparable:

| arm | PASS | FAIL | ABORTED | NOTESTS |
|---|---:|---:|---:|---:|
| Generational | 447 | **40** | 7 | 30 |
| G1 | 465 | 22 | 7 | 30 |
| ZGC | 468 | 19 | 7 | 30 |

**19 classes fail on Generational and pass on both G1 and ZGC:**

```
io.netty.buffer.AdvancedLeakAwareByteBufTest
io.netty.buffer.AdvancedLeakAwareCompositeByteBufTest
io.netty.buffer.BigEndianCompositeByteBufTest
io.netty.buffer.BigEndianDirectByteBufTest
io.netty.buffer.BigEndianHeapByteBufTest
io.netty.buffer.DuplicatedByteBufTest
io.netty.buffer.LittleEndianCompositeByteBufTest
io.netty.buffer.LittleEndianHeapByteBufTest
io.netty.buffer.PooledBigEndianHeapByteBufTest
io.netty.buffer.PooledLittleEndianDirectByteBufTest
io.netty.buffer.PooledLittleEndianHeapByteBufTest
io.netty.buffer.ReadOnlyDirectByteBufferBufTest
io.netty.buffer.RetainedDuplicatedByteBufTest
io.netty.buffer.SimpleLeakAwareByteBufTest
io.netty.buffer.SimpleLeakAwareCompositeByteBufTest
io.netty.buffer.SlicedByteBufTest
io.netty.buffer.WrappedCompositeByteBufTest
io.netty.channel.nio.NioEventLoopTest
io.netty.handler.ipfilter.UniqueIpFilterTest
```

Two more differ without being Generational-only, and are **not** covered by this
page: `io.netty.util.RecyclerTest` fails on **G1 only**;
`DefaultPromiseTest` and `JdkDelegatingPrivateKeyMethodTest` fail on
Generational **and** G1 while passing on ZGC.

## 2. The symptom is always the same, and it is always a concurrent test

Every one of the 66 individual failures across the buffer family is a
`*MultipleThreads` / concurrent test, in four methods:

| failures | test |
|---:|---|
| 17 | `testDuplicateReadGatheringByteChannelMultipleThreads()` |
| 15 | `testSliceReadGatheringByteChannelMultipleThreads()` |
| 14 | `testDuplicateReadOutputStreamMultipleThreads()` |
| 11 | `testSliceReadOutputStreamMultipleThreads()` |
| 3+3+2+1 | `testCopyMultipleThreads0()`, `repetition`, `testConcurrentUsage()`, `testCopyMultipleThreads()` |

and the exception is never in netty. It is inside JUnit's own machinery:

```
java.lang.NullPointerException
  at org.junit.jupiter.engine.execution.InvocationInterceptorChain$ValidatingInvocation
       .verifyInvokedAtLeastOnce(InvocationInterceptorChain.java:148)
  at ...InvocationInterceptorChain.chainAndInvoke(InvocationInterceptorChain.java:46)
```

i.e. **a reference field of a live JUnit object reads back null.** That is a
corrupted-heap signature, not a test assertion.

## 3. Four arms, one class, and the lever

`io.netty.buffer.DuplicatedByteBufTest`, standalone (no shard contention — a
FAIL family on a loaded host can be the host, so this is the first thing
checked):

| arm | result |
|---|---|
| **HotSpot**, identical classpath | `found=416 ok=416 failed=0` in **10.7 s** |
| CratonVM **ZGC** | 0/3 reps failing |
| CratonVM **G1** | passes (suite arm) |
| CratonVM **Generational** | **3/3 reps failing** — `ok=413 failed=3`, 143-153 s |
| Generational + `CRATONVM_NO_MOVING_YOUNG=1` | **0/3 reps failing** |

Three classes were run in that shape (`DuplicatedByteBufTest`,
`BigEndianHeapByteBufTest`, `SimpleLeakAwareByteBufTest`): **9/9 failing on
Generational, 0/9 with the lever, 0/9 on ZGC.**

## 4. Engagement census — the moving cycle is real, and rare

A lever that removes a failure also changes timing, so "it went away" is not by
itself an attribution. `CRATONVM_GC_STATS=1` on the failing arm:

```
[GC] decision histogram: moving=9 non_moving=366
     moving-jit-coverage-proven=9 nonmoving-coverage-incomplete=366
[GC] decision history: moving_cycles_under_live_jit=9 coverage_fallbacks=366
```

**Nine cycles genuinely relocate**, every one of them under live JIT and every
one self-certified `moving-jit-coverage-proven`. Two or three tests of 416 then
fail. A defect that needs one of nine rare cycles to hit a live object is
exactly the shape of an intermittent, shape-shifting corruption.

The other 366 cycles fall back (`unregistered` 338, `innermost` 28,
`nonmoving` 1). **Those fallbacks are a different story** — see §5.

## 5. What this is NOT

Two existing pages describe adjacent things, and this is neither. Both were
checked against the lever before being ruled out.

* **Not the DoHead Generational story.**
  `docs/known-issues/tomcat/dohead-family-consolidated-history.md` describes the
  `[moving-young] fallback` mechanism — the 366 — and concludes *"Not a
  correctness bug"* and *"No action needed on the correctness front."* That
  conclusion is sound for the mechanism it describes: falling back to the
  non-moving sweep is safety-first and costs throughput. **It does not cover
  this.** Here the failures track the **9 cycles that did NOT fall back**, and
  the result is a wrong answer rather than a slow one, with HotSpot clean on the
  identical classpath.

* **Not HIB-CV-22.** That page
  (`HIB-CV-22-junit-timeoutextension-double-invoke-is-gc-corruption.md`) has the *same victim class* —
  it is the same JUnit `ValidatingInvocation` object — and even predicts this
  exact face (*"a reference field zeroed ⟶ NullPointerException"*). But its
  corruptor is the **non-moving** sweep, and its discriminator is that the
  failure **vanishes with `CRATONVM_DBG_FORCE_MOVING=1`**. This one vanishes
  with the opposite lever. Same casualty, different corruptor; the shared JUnit
  stack is a coincidence of which object happens to be young, small, and
  allocated once per test invocation.

## 6. Where to look next

The nine cycles all claim `moving-jit-coverage-proven` under live JIT, so the
first question is whether that proof is sound for these frames. Prior art says
to distrust it: `moving-young-corruption-rootcause.md` root-caused an earlier
instance as *precise-only under-coverage* — the coverage bit was computed from
locals + operand stack while a compiled frame also holds oops in
scalar-replacement slots, LICM hoist slots and the blind GPR spill area — and
this codebase has repeatedly found coverage proofs that certify a frame they
never actually inspected.

Not attempted here: naming the corrupted slot. The instrument that would do it
is a watch on the JUnit object at allocation (`CRATONVM_DBG_WATCH_ALLOC_CID`),
the same one that closed the 2026-09-06 TLAB-skip-span defect.

## 7. Reproducing

```bash
CV=<cratonvm.exe>
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
ARGS=C:/craton/CratonVM1/apps/netty-suite-runner/common-cvm1.args

# fails 3/3, ~150 s
"$CV" --java-home "$JDK" --Xmx 1g -XX:+UseGenerationalGC "@$ARGS" \
      -Dcraton.batch=1 CratonRunner io.netty.buffer.DuplicatedByteBufTest

# clean 3/3
CRATONVM_NO_MOVING_YOUNG=1 "$CV" ... (as above)

# HotSpot oracle: 416/416 in 10.7 s
"$JDK/bin/java.exe" "@$ARGS" -Dcraton.batch=1 CratonRunner \
      io.netty.buffer.DuplicatedByteBufTest
```

**Coverage limit of the run behind this page:** the three-collector sweep was
interrupted at 590/538/586 of 657 classes, so 133 classes are missing from at
least one arm and are excluded from every count above. The 19-class list is
therefore a **lower bound** — the remaining classes are mostly the known
long-running/hanging ones, and some of them may add to it.
