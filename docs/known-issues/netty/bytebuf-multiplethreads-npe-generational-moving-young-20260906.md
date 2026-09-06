# 19 netty classes fail on **Generational only** — the moving young cycle corrupts a live object

**Status:** OPEN, **root cause identified** (2026-09-06). Found by a
per-collector sweep of the full netty suite. **This is a correctness defect,
not a throughput one**, and it is invisible to every run that uses the shipped
default collector.

**One line:** a moving young cycle copies an object into to-space and never
scans it, so every reference slot in that one object keeps pointing into
from-space. See §6 for the named slots and §7 for the amplifier that reproduces
it 3/3.

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

## 6. Root cause — a to-space survivor that is copied but never SCANNED

`CRATONVM_MOVING_YOUNG_VERIFY=1` runs a post-evacuation pass that reports any
surviving heap reference still pointing at a forwarded young-from object. Its
own contract splits the diagnosis in two: **non-zero means a heap reference
rewrite was missed; zero with a wrong result means the stale reference lives
outside the heap** (an unenumerated root or compiled-frame home).

It comes back **non-zero**, and it names the victims. Two reps under the
amplifier, both failing 3/416 with the NPE:

```
rep 0  forwarded_heap_refs_remaining young=4 old=0
  MISSED-HEAP-REWRITE YOUNG TestTemplateExtensionContext@0x17590030b60
      slot=24 -> UnmodifiableSet             old=0x175826b0e78 new=0x175900268c0
      slot=40 -> DefaultExecutableInvoker    old=0x175826b0568 new=0x1759002 68f0
      slot=48 -> MutableExtensionRegistry    old=0x175826af9f8 new=0x17590026838
      slot=64 -> NamespacedHierarchicalStore old=0x175826b0ea8 new=0x17590026910

rep 1  forwarded_heap_refs_remaining young=2 old=0
  MISSED-HEAP-REWRITE YOUNG ConcurrentHashMap@0x204d5f028b8
      slot=0 -> Object                       old=0x204e958e088 new=0x204d5ef8d68
      slot=2 -> ConcurrentHashMap$Node       old=0x204e968ed50 new=0x204d5ef8d98
```

**So it is not a missed root.** Every referent was found, copied, and given a
new home — the collector knew about all of them. What was not done is the
referrer's own slot rewrite, and in each cycle **every missed slot belongs to a
single object**. An object whose referents all moved and none of whose slots
were updated was copied into to-space and then **never scanned**.

The addresses agree: in both reps the un-scanned referrer sits at a HIGHER
address than the new homes of the objects it points at
(`0x…30b60` vs `0x…268c0-26910`; `0x…f028b8` vs `0x…ef8d68-ef8d98`). Cheney
scans to-space in address order, so a referrer above the region where its
referents were placed should have been scanned after them and rewritten. It
reads as a scan cursor that finished before reaching the object — i.e. the
object was copied after the scan loop believed it was done.

Only one of the 22-29 moving cycles in a run reports a miss, which is why the
symptom is 2-3 failures in 416 tests rather than a crash.

**Stated as a hypothesis, not a conclusion:** the address ordering is
consistent with a late copy that no re-scan followed, but this has not been
confirmed by instrumenting the scan cursor itself. That is the next
measurement, and it is a small one — record the final to-space scan cursor
beside each `MISSED-HEAP-REWRITE` and check the referrer is above it.

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

## 8. Status of the older lead

The pre-root-cause suspicion was *precise-only under-coverage* -- that
`moving-jit-coverage-proven` certifies a compiled frame whose oops live in
scalar-replacement slots, LICM hoist slots or the blind GPR spill area, per
`moving-young-corruption-rootcause.md`. **The verifier rules that out as the
mechanism here**: a missed compiled-frame home would leave the heap consistent
and show `forwarded_heap_refs_remaining young=0`, with the stale reference
outside the heap. It reports non-zero, in the heap, in one object. The coverage
proof is still the gate that lets the cycle run (§7) -- it is not the thing that
loses the pointer.

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
