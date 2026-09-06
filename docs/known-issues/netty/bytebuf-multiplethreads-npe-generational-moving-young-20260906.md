# 19 netty classes fail on **Generational only** — the moving young cycle corrupts a live object

**Status:** OPEN. Found by a per-collector sweep of the full netty suite.
**This is a correctness defect, not a throughput one**, and it is invisible to
every run that uses the shipped default collector.

**One line:** relocation is necessary for the failure, and the stale reference
is **not in the heap** — so it is held by an unenumerated root or a compiled
frame home. §7 has the amplifier that reproduces it 3/3.

> **An earlier revision of this page claimed the root cause was "a to-space
> survivor copied but never scanned". That claim is WITHDRAWN — see §6.** It
> rested on two observations of a missed heap rewrite; instrumenting the scan
> cursor refuted it across 80 moving cycles.

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

## 4. Engagement census — relocation is real, and necessary

A lever that removes a failure also changes timing, so "it went away" is not by
itself an attribution. `CRATONVM_GC_STATS=1` on the failing arm:

```
[GC] decision histogram: moving=9 non_moving=366
     moving-jit-coverage-proven=9 nonmoving-coverage-incomplete=366
[GC] decision history: moving_cycles_under_live_jit=9 coverage_fallbacks=366
```

**Cycles genuinely relocate** — nine in that run, and between 1 and 24 across
every run measured since — every one of them under live JIT and every one
self-certified `moving-jit-coverage-proven`. Two to four tests of 416 then fail.

The count is reported as a range on purpose. Relocation is **necessary** for the
failure: remove it (`NO_MOVING_YOUNG`, `HANDSHAKE=0`) and the family goes clean,
force more of it (`ASSUME=1`) and the failures go to 3/3. What has NOT been
established is a per-cycle link — no measurement here says which relocating
cycle corrupted which object, and an earlier revision of this page overreached
by writing as though the count and the failures tracked each other one-to-one.

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
  this.** Here the failure REQUIRES the cycles that did NOT fall back — kill
  relocation and it goes away — and the result is a wrong answer rather than a
  slow one, with HotSpot clean on the identical classpath.

* **Not HIB-CV-22.** That page
  (`HIB-CV-22-junit-timeoutextension-double-invoke-is-gc-corruption.md`) has the *same victim class* —
  it is the same JUnit `ValidatingInvocation` object — and even predicts this
  exact face (*"a reference field zeroed ⟶ NullPointerException"*). But its
  corruptor is the **non-moving** sweep, and its discriminator is that the
  failure **vanishes with `CRATONVM_DBG_FORCE_MOVING=1`**. This one vanishes
  with the opposite lever. Same casualty, different corruptor; the shared JUnit
  stack is a coincidence of which object happens to be young, small, and
  allocated once per test invocation.

## 6. WITHDRAWN root cause, and what the instrument actually proved

The first attempt at a root cause was wrong, and the way it was wrong is worth
keeping.

`CRATONVM_MOVING_YOUNG_VERIFY=1` reports any surviving heap reference still
pointing at a forwarded young-from object. Its contract splits the diagnosis
cleanly: **non-zero = a HEAP rewrite was missed; zero with a wrong result = the
stale reference lives OUTSIDE the heap** (an unenumerated root or compiled-frame
home).

Two runs came back non-zero and named the slots — every miss in a cycle
belonging to one object, whose referents had all been copied and rehomed:

```
rep A  young=4 old=0   TestTemplateExtensionContext@0x17590030b60
                         slot=24 -> UnmodifiableSet, slot=40 -> DefaultExecutableInvoker,
                         slot=48 -> MutableExtensionRegistry,
                         slot=64 -> NamespacedHierarchicalStore
rep B  young=2 old=0   ConcurrentHashMap@0x204d5f028b8  slot=0, slot=2
```

An object whose every slot was missed looked like an object copied into
to-space and never scanned, and the referrers sat at HIGHER addresses than
their referents' new homes — consistent with a Cheney scan cursor that finished
early. The mechanism was plausible: the main drain runs to the `used()` of its
moment, phase 2.5 then resurrects finalizable objects (copying MORE into
to-space), and the re-drain after it is guarded by
`if !dead_finalizers.is_empty()`.

**So the cursor was instrumented, and it refutes the story.** Every
`[moving-young-verify]` line now carries the frontier:

```
[moving-young-verify] scan frontier: scan_cursor=0x15fd30 to_used=0x15fd30 \
                      unscanned_tail=0x0 dead_finalizers=0
```

Five reps under the amplifier:

| rep | NPE failures | moving cycles | cycles with a missed rewrite | cycles with `unscanned_tail > 0` |
|---:|---:|---:|---:|---:|
| 0 | 4 | 18 | 0 | 0 |
| 1 | 3 | 15 | 0 | 0 |
| 2 | 3 | 17 | 0 | 0 |
| 3 | 3 | 20 | 0 | 0 |
| 4 | 4 | 10 | 0 | 0 |

**80 moving cycles. The scan cursor reached `to_used` on every one of them, and
not one missed a heap rewrite — while the NPE fired 17 times.** Two conclusions,
and the second is the useful one:

1. The early-cursor hypothesis is dead. The Cheney drain completes.
2. **A missed heap rewrite is NOT necessary for the failure.** It is real — it
   was observed twice — but it is rare and it is a second signature, not the
   cause. A mechanism absent while the symptom is present is not the mechanism.

**What the zero therefore means, by the verifier's own contract: the stale
reference is outside the heap.** The heap is self-consistent after evacuation;
something that is not a heap slot still holds the pre-move address.

The methodological error is worth naming: the withdrawn claim was published off
two observations of a signature that turned out to be neither necessary nor
typical, and the confirming instrument was built only afterwards. The
instrument is what should have come first.

## 7. Why the coverage proof does not stop it

`moving-jit-coverage-proven` is emitted on `has_conservative_roots` — "a JIT
frame was live" — not on any peer proof. The peer ledger that would be a proof
is `refresh_moving_young_coverage_for_collection`, and on this workload **it
never accepts**: 0 `accounted=true` out of 750 decisions, minimum
`peer_depth` 4. Two levers bracket it on one binary:

| arm | failing | moving cycles |
|---|---:|---:|
| baseline | 1/3 | 1 |
| `CRATONVM_XT_JIT_COVERAGE_HANDSHAKE=0` (force refuse) | **0/3** | 0 |
| `CRATONVM_XT_PINNED_PEER_DEPTH=0` | 1/3 | 0 |
| `CRATONVM_XT_JIT_COVERAGE_ASSUME=1` (force accept) | **3/3** | **14-24** |

`ASSUME=1` is a **deliberate amplifier**: it turns an intermittent 1-in-3 defect
into a 3/3 one and is what made the verifier catch the miss. Use it to
reproduce.

Two candidate doors were tested and **refuted**, so nobody re-runs them:

* **The pin credit is not it.** `pinned=0` in all 750 samples and
  `XT_PINNED_PEER_DEPTH=0` changes nothing.
* **`peer_depth` is not torn.** `peer_jit_depth()` reduces a striped counter
  with `saturating_sub`, whose comment calls zero "the safe reading" — and zero
  skips the whole ledger, so it looked like an unguarded door. Counters added
  for this say otherwise: of the `peer_depth == 0` cycles, `global_zero=0` and
  `torn_global_lt_local=0`, i.e. `global == local` every time — a consistent
  reading meaning only the initiator was in compiled code.

## 8. The indicated cause: precise-only under-coverage

An earlier revision of this page said the *precise-only under-coverage* lead in
`moving-young-corruption-rootcause.md` was "ruled out". **That was wrong**, and
it was ruled out on exactly the evidence that has since been withdrawn.

That page root-caused an earlier instance as a coverage bit computed from the
abstract interpreter's locals + operand stack, while a compiled frame also
holds oops in **scalar-replacement slots**, **LICM hoist slots** and the
**blind GPR spill area**. A stale reference in any of those is invisible to the
heap verifier and survives evacuation un-rewritten — which is precisely the
`young=0 old=0` + wrong-result signature §6 now reports 5/5.

It also fits everything else: the failures need relocation (§3), they need
peers (every failing test is `*MultipleThreads`), and forcing the peer ledger to
accept amplifies them to 3/3 (§7) — a ledger whose whole job is to decide
whether peers' compiled frames are rewritable.

Next measurement, in order:

1. `CRATONVM_MOVING_YOUNG_BAND_DBG=1` — dump the frame offset of every word the
   band verifier rejects, on a cycle that relocates under the amplifier. That
   names the frame slot the same way §6's instrument named heap slots.
2. The `CRATONVM_MOVING_YOUNG_NO_BAND_*` screens, one at a time, to find which
   screen admits the un-rewritable word.
3. Only then a fix. The prior art is explicit that the real repair is precise
   oop maps or a shadow stack, not a policy patch.

## 9. Reproducing

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
