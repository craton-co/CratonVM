# 19 netty classes fail on **Generational only** — the moving young cycle corrupts a live object

**Status:** OPEN, but **currently MASKED on dev — see §0.** Found by a
per-collector sweep of the full netty suite.
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

## 0. It no longer reproduces on dev, and that is NOT a fix (2026-09-06, late)

Re-checked on dev `da44c949a`, 80 commits after the binary this page was
written against. Four of the 19 classes, 3 reps each, concurrent, plain
`-XX:+UseGenerationalGC` — no amplifier, no verifier, the configuration a user
would actually run:

| class | NPE | timeouts | moving cycles |
|---|---:|---:|---:|
| `DuplicatedByteBufTest` | 0 | 0 | 1 |
| `BigEndianHeapByteBufTest` | 0 | 0 | 1 |
| `SimpleLeakAwareByteBufTest` | 0 | 0 | 0 |
| `SlicedByteBufTest` | 0 | 0 | 0 |

**12 runs, zero failures.** And the reason is in the last column: **the moving
young cycle has stopped running on this workload.** This morning the same
workload took 9-24 moving cycles per run across four separately-built binaries;
it now takes 0-1, and the collector says why:

```
histogram: moving=0 non_moving=314 nonmoving-conservative-jit-roots=15
                                   nonmoving-coverage-incomplete=299
reason=unregistered-jit-frame-on-stack   (12 of 14 sampled)
```

Relocation is necessary for this defect (§3, §4). Relocation no longer happens,
so the defect no longer fires. **Nothing here shows the corruption was fixed —
it shows it is no longer exercised.**

That has a specific and uncomfortable consequence. The refusal now dominating
the histogram is the SAME mechanism the DoHead page documents as a
Generational-only *throughput collapse* and dismisses as "not a correctness
bug". On this workload that throughput bug is currently the only thing standing
between the user and this correctness bug. **Whoever repairs moving-young
engagement — the obvious and desirable performance fix — re-exposes this.** The
green above is conditional on a collector declining to do its job.

Not bisected: the shift is consistent across four binaries built today on one
host, so it is attributed to dev movement rather than host state, but no arm
rebuilt the old commit to confirm it. Two commits in this exact machinery landed
in the window (`70c486744` peer pin credit, `65e7bffc2` peer pins G1-only).

### The obligation/flag mismatch, resolved — and what it exposes

The decision line reads `unproven_obligation=unregistered-jit-frame-on-stack`
while the live flag in the SAME line reads `unregistered_jit_frame=false`. It
was filed here as a possible stale obligation. **It is not.** Both are set
together at one site (`conservative_roots.rs`, the unregistered-frame branch),
and both are reset once per collection. The reason they disagree is that they
have **different scopes**:

```rust
#[cfg(not(test))] static MOVING_YOUNG_COVERAGE_INCOMPLETE: AtomicBool   // PROCESS-GLOBAL
#[cfg(not(test))] static MOVING_YOUNG_INCOMPLETE_REASON:   AtomicUsize  // PROCESS-GLOBAL
                  thread_local! { UNREGISTERED_JIT_FRAME: Cell<bool> }  // PER-THREAD
```

The verdict and its reason are process-global first-wins atomics; the flag is
the collecting thread's own. So the line is reporting a reason **some other
thread** recorded next to a flag that only ever describes this one. Nothing is
stale; two differently-scoped facts are printed as though they were one.

**The substantive consequence.** `refresh_moving_young_coverage_for_current_thread`
runs on every mutator at its root-snapshot deposit, and a mutator whose own
frames are unproven sets the GLOBAL verdict. **One thread failing its own proof
therefore refuses relocation process-wide for that cycle** — on a workload with
many event-loop threads, that needs only one. This is the same blanket
"any peer in JIT means unproven" behaviour the cross-thread handshake was built
to replace (§7), re-entering through a different door: not the peer ledger, but
the global verdict every thread can set. It is a plausible mechanism for the
moving-cycle collapse in the table above, and it is fail-closed, so it costs
throughput rather than correctness.

**And a coverage gap worth fixing on its own.** Under `#[cfg(test)]` those same
two statics become `thread_local!` cells. The stated reason is sound — stop one
test's deliberate "incomplete" from diverting another's collection — but the
effect is that **every unit test of this decision path exercises per-thread
semantics that production does not have.** No test in this machinery can
observe one thread refusing another's cycle, which is precisely the behaviour
that matters in a multi-threaded collector.

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

## 10. 2026-09-06, later still: the mask has a name, §7's amplifier is dead, and there is now a repro that needs no unsafe flag

Added by a second session working this page in parallel. Nothing above is
deleted; three things in it are corrected, and the corrections are all
measurements on one binary per arm.

Binaries: dev `6430e495f` (`cvm-gy-20260906`, Linux; and a Windows build of the
same commit) and the merged tip `c36129c34` / dev `f45fe11d0`
(`cvm-gy2-20260906`, Linux).

### 10.1 The mask is `unrewritable_conservative_jit_roots`, and it has a kill switch

§0 says relocation stopped and does not say what stopped it. It is a term added
to `gen_heap::collect_garbage_inner` on 2026-09-06, under the heading *"A CHEENY
COPY CANNOT HONOUR A PIN"*:

```rust
let unrewritable_conservative_jit_roots = moving_young
    && !gen_no_peer_pin_divert()
    && has_conservative_roots
    && crate::gc_quiescence::conservative_jit_scans() > 0;
```

Any cycle handed a conservatively-discovered JIT root while a compiled frame is
live now diverts, reported as `nonmoving-conservative-jit-roots` — which is
exactly the reason code §0 saw appear at 15 per run. It was landed against a
QDox 4-thread SIGSEGV repro (3/3 → clean) and **it ships with a one-binary kill
switch, `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`.** That switch is the tool this page
has been missing.

### 10.2 §7's `ASSUME=1` amplifier no longer amplifies — do not build on it

`CRATONVM_XT_JIT_COVERAGE_ASSUME=1`, three reps on the merged tip,
`DuplicatedByteBufTest`:

| rep | NPE frames | moving cycles |
|---:|---:|---:|
| 1 | 0 | **0** |
| 2 | 0 | **0** |
| 3 | 0 | **0** |

The divert in §10.1 sits **downstream** of the peer ledger, so forcing the
ledger to accept changes nothing: the cycle is refused one term later. §7's
table and §9's "use it to reproduce" are stale, and any future measurement that
uses `ASSUME` as its amplifier will be vacuous.

Two further readings from the same runs, with `CRATONVM_DBG_XT_COVERAGE=1`:

* **the peer ledger never legitimately accepts.** `proven < peer_depth` on
  essentially every decision — 13/13, 22/22, 15/16, 19/19, 20/22. §7 already
  says "0 `accounted=true` out of 750"; this adds *why*, and the consequence
  matters for §8. **Every failure this page has ever measured came from
  relocating under peers that proved nothing.** "The precise maps are
  incomplete" and "we relocated under a peer that never deposited a proof" are
  different bugs, and no arm to date has separated them.

### 10.3 The pin credit WAS a door — §7 refuted it with the wrong-direction switch

§7 says *"The pin credit is not it. `pinned=0` in all 750 samples and
`XT_PINNED_PEER_DEPTH=0` changes nothing."* Both observations are correct and
neither is evidence, because **the credit is already off**: `70c486744` (2026-09-06)
made `pinned_credit_admissible` require `pins_honoured`, and
`VmHeap::honours_conservative_pins()` is `false` for Generational. Setting
`XT_PINNED_PEER_DEPTH=0` turns a zero into a zero.

The switch that moves is the one that commit added for exactly this purpose,
`CRATONVM_XT_PINNED_PEER_UNPINNABLE=1`. On dev `6430e495f`, one binary per
platform, three classes × three reps:

| platform | arm | reps failing | moving cycles / rep |
|---|---|---:|---|
| Windows | default | **0 / 9** | 0–1 |
| Windows | `…UNPINNABLE=1` | **9 / 9** | 13–25 |
| Azure Linux | default | **0 / 9** | 0–2 |
| Azure Linux | `…UNPINNABLE=1` | **9 / 9** | 6 |

`DuplicatedByteBufTest` returns `ok=413 failed=3` — this page's own numbers — in
the same four `*MultipleThreads` methods with the same
`verifyInvokedAtLeastOnce` NPE. Across the full 19-class list, one rep each:
**0 of 19 classes fail by default, 17 of 19 fail with the switch (52 individual
tests).** `CRATONVM_DBG_XT_COVERAGE=1` shows the mechanism rather than inferring
it:

```
default      [xt-coverage] peer_depth=5 proven=1 pinned=0 pins_honoured=false accounted=false
UNPINNABLE   [xt-coverage] peer_depth=5 proven=1 pinned=4 pins_honoured=false accounted=true
```

238 cycles accepted *because of* a pin on the switched arm, 0 on the default.

This also supplies the bisect §0 records as missing ("no arm rebuilt the old
commit to confirm it"): `70c486744` is one of the two commits §0 names, and this
is it, isolated on one binary.

### 10.4 The repro this page should use from now on

**`CRATONVM_GC_NO_PEER_PIN_DIVERT=1`, `io.netty.handler.ipfilter.UniqueIpFilterTest`,
merged tip, plain `-XX:+UseGenerationalGC --Xmx 1g`.** No unsafe flag, ~25 s a
rep, ~30 relocating cycles per run, and it crashes:

| batch | reps | SIGSEGV | proven relocating cycles on the clean reps |
|---|---:|---:|---|
| soak | 5 | **1** | 29–39 |
| naming probe | 8 | **2** | 21–37 |

**3 of 13, and the pin credit is OFF in all of them.** That is the arm §8 needs
and did not have: these cycles got through on the coverage proof itself, so this
is the first measurement on this page that actually exercises the precise maps
rather than bypassing them. §8's direction survives it.

The Linux face is harder than the Windows NPE, and the crash handler names the
shape without help:

```
#  SIGSEGV at pc=0x7407b1afff35, addr=0x7407cabdbc8f
#  fault addr is inside a RECENTLY DECOMMITTED heap span: base=0x7407c2c00000 len=0xfa00000 site=unbumped-middle
#  fault pc is inside a LIVE registered code buffer: base=0x7407b1aff000 cap=0x6e40
```

Compiled code dereferencing a stale pointer into evacuated space — the same
"stale reference outside the heap" §6 concluded, now with the reader identified.

Adding the pin credit on top (`NO_PEER_PIN_DIVERT=1` **and**
`XT_PINNED_PEER_UNPINNABLE=1`) takes it to **SIGSEGV 6/6** in 3–39 s across two
classes; each switch alone is far milder. So the two doors compose, and neither
is the whole story.

### 10.5 What the stale-word census names, and why its `resumed_from` zero is not an all-clear

`CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1` under the §10.4 repro — the first time
this instrument has run against a workload that actually relocates, and it is
the "name the storage class" step §8 asks for. Words still naming a moved-from
address AFTER the remap, per rep, 8 reps:

| `FrameLayout` region | verifiable=false | verifiable=true |
|---|---:|---:|
| `operand-spill` | 779–1400 | 211–703 |
| `safepoint-gpr-spill-image` | 507–1062 | 0 |
| `outgoing-args-or-deopt-regs` | 370–796 | 0 |
| `java-local` | 0 | 24–54 |
| `callee-saved-gpr-image` | **0** | **0** |

Two things to carry forward, and the second is a caveat on the first:

* **The 2026-08-23 repair holds.** `callee-saved-gpr-image` — the region that
  bug was about — is zero in all eight reps. And
  `CRATONVM_MOVING_YOUNG_VERIFY=1` reports 0–1 missed heap rewrites per rep,
  reproducing §6's result on this configuration.
* **`resumed_from=0` must NOT be read as "nothing resumes from these words".**
  That flag is computed as `is_callee_saved_gpr_image(...)` and nothing else, so
  it is `false` by construction for every row in the table above. It says "not
  in the region we already fixed", not "harmless". Before this census can be
  acted on, that classifier has to be widened to the regions something really
  does resume from — at minimum the safepoint GPR spill image and reloaded
  operand-spill slots.

A second caveat on the raw counts: the detector reports any word whose value is
a key of `pointer_map`, and dead stack slop holding an old object address
matches. Thousands per run is therefore an upper bound on candidates, not a
count of live stale references. Separating the two is what the widened
`resumed_from` would buy, and it is the next measurement.

### 10.6 Suite-wide state at the tip, and three corrections to §1's neighbours

The full 733-class netty suite, one binary, Generational; then every class that
did not PASS re-run on G1 and ZGC. **`NullPointerException` does not appear
anywhere in the entire Generational arm** (against a working instrument: 2357
`TestAbortedException`, 716 `NoClassDefFoundError`, 78 `UnsatisfiedLinkError`
all enumerate normally). Eight classes looked Generational-only; re-run cleanly
standalone, all eight dissolve:

* five were **my own artifacts** — `process-died rc=143`, SIGTERM with zero
  output, from killing a sweep-chain parent and from a watchdog whose `ps`
  column parse read elapsed time as `4123168608s`. A family assembled from those
  rows would have been fiction, which is the reason they are named here;
* two (`LittleEndianHeapByteBufTest`, `PooledLittleEndianHeapByteBufTest`) were
  HANG at the sweep's flat 180 s cap and finish in 71–75 s standalone;
* `ChannelInitializerTest`'s one failure **also fails on HotSpot** on the
  identical classpath.

`NioEventLoopTest` (§1's list) is worth a line for the same reason: on the tip
its 13 tests pass in 17 s and the VM then never exits. **HotSpot does exactly
the same** — `ok=13 failed=0` in 5.7 s, then killed at its own cap. The non-exit
is the fixture's non-daemon event loop, not a CratonVM defect.

The three classes §1 lists as *not* covered by this page, one run each per
collector on the tip:

| class | Generational | G1 | ZGC | §1 said |
|---|---|---|---|---|
| `RecyclerTest` | failed=0 | **failed=6** | failed=0 | G1 only — **confirmed**, and the failures are `AssertionFailedError: expected: <3> but was: <4>`, a recycler count |
| `DefaultPromiseTest` | **failed=1** | failed=0 | failed=0 | Gen **and** G1 — **moved**; the one failure is `TimeoutException: testListenerNotifyOrder() timed out after 120 seconds` |
| `JdkDelegatingPrivateKeyMethodTest` | failed=0 | failed=0 | failed=0 | Gen **and** G1 — **now green everywhere** |

### 10.7 Where this leaves the page

Still **OPEN**, and §8's direction is now supported by an arm that tests it
instead of bypassing it. The order of work has changed, though:

1. Widen `resumed_from` (§10.5) so the stale-word census can be read. Until
   then it names candidate regions and cannot rank them.
2. Then §8's screens, using `CRATONVM_GC_NO_PEER_PIN_DIVERT=1` as the
   amplifier — **not** `ASSUME=1`, which no longer moves anything.
3. Note for whoever repairs moving-young engagement, restating §0's warning with
   its mechanism attached: the thing standing between a user and this bug is
   `unrewritable_conservative_jit_roots`, and it is a deliberate correctness
   term with a QDox repro behind it, not an accident. Removing it re-exposes
   this at ~25 % per run on `UniqueIpFilterTest`.

### 10.8 §8's blind-spill candidate is eliminated, and the surviving lead is `region=unclassified`

`moving-young-corruption-rootcause.md` nominates three storage classes, and §8
adopts them: **scalar-replacement slots**, **LICM hoist slots**, and the **blind
GPR spill area**. One of the three can now be crossed off.

`CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE=1` (added with this section) widens the
register-image remap from the callee-saved GPR image alone to the entire
unverifiable tail — so `operand-spill`, `safepoint-gpr-spill-image` and
`outgoing-args-or-deopt-regs` all get rewritten. On the §10.4 repro:

| arm | crashes / runs | rate |
|---|---|---|
| default | **11 / 81** | 13.6 % |
| widened | **2 / 32** | 6.3 % |

Fisher p ≈ 0.37 — **no difference**. Rewriting every one of those regions does
not move the crash rate, so none of them holds the reference that faults. That
eliminates the blind GPR spill area, and it also retires the worry in §10.5 that
the narrow `resumed_from` classifier was hiding a live stale word in the other
two: if one were live, writing it would have helped.

**A methodological note, because this nearly went in as a finding.** The first
batch read narrow 0/14 vs wide 2/14 and I wrote it up as "widening makes it
worse". The pooled default rate is 13.6 %, so 2/14 *is* the baseline and 0/14
was the outlier — and the second matched batch came back narrow 2/18 vs wide
0/18, i.e. the same null with the arms swapped. Neither batch means anything
alone. The pooled counts are the finding; a 14-rep arm against a ~14 % event is
not an arm.

**What survives.** The stale-word census (§10.5) has one population small enough
to be real rather than dead slop: `region=unclassified`, at **0–6 words per
rep** against thousands for every named region. They sit at fixed frame offsets
in specific compiled methods —

```
method=io/netty/channel/DefaultChannelPromise.setSuccess:()Lio/netty/channel/ChannelPromise;      off=448
method=io/netty/channel/ChannelInitializer.initChannel:(Lio/netty/channel/ChannelHandlerContext;)Z off=528
method=io/netty/channel/AbstractChannelHandlerContext.findContextInbound:(I)L…;                    off=544
```

— and `FrameLayout::region_name` cannot place them, which is precisely what a
**scalar-replacement or LICM hoist slot** would look like to a classifier that
does not know those regions: §8's other two candidates, and the only ones left.
Some rows even resolve a class (`io/netty/channel/embedded/…`), so they are not
all noise.

Stated honestly: the crashing reps carried 5 and 4 such words and two clean reps
carried 0 — but another clean rep carried 6, so this is a **lead, not a
correlation**, and it wants the per-cycle pairing (does a crash follow a cycle
that left one of these behind?) rather than a per-run count.

Next measurement, replacing §8's list:

1. Teach `FrameLayout::region_name` the scalar-replacement and LICM hoist
   spans, so `unclassified` resolves into one of them or stays genuinely
   unknown. Right now the census cannot tell those two candidates apart, and
   they are the last two standing.
2. Pair the census with the fault per CYCLE rather than per run.
3. Do **not** re-run the widening arm; §10.8 is 113 VM launches and the answer
   is null.

### 10.9 Correction to §10.8: the scalar/LICM zero is VACUOUS, and `unclassified` means something else

§10.8 said the `region=unclassified` words are the surviving lead because
`FrameLayout::region_name` "cannot place them, which is exactly how a
scalar-replacement or LICM hoist slot would present". **Both halves of that are
wrong, and the correction points somewhere different.**

**`region_name` already classifies all three.** Its ladder names
`scalar-replaced-field`, `licm-ref-hoist` and `licm-arith` before it reaches
anything else. So an unnamed word is not an unnamed scalar/hoist slot; those
have names and would have used them.

**And their zero is vacuous, which is worse.** Across every log this branch
produced — 8 naming-probe reps plus every other armed run — the tally is:

```
13064 region=operand-spill
 6071 region=safepoint-gpr-spill-image
 5620 region=callee-saved-gpr-image      (these are [jit-register-image-remap]
 4829 region=outgoing-args-or-deopt-regs  lines, i.e. the 2026-08-23 repair
  315 region=java-local                   doing its job, not stale words)
   21 region=unclassified
```

`scalar-replaced-field`, `licm-ref-hoist`, `licm-arith` and
`reserved-locals-tail`: **zero occurrences, in either stream.** That is not
evidence they are clean. `x64/frames.rs` builds `scalar_lo/scalar_hi` from
`self.scalar_replaced` and `ref_hoist_lo/ref_hoist_hi` from
`self.hoist_offsets`, both of which are `(0, 0)` when the optimisation produced
no slots — and `region_name`'s `hit()` requires `hi > lo`. **On a frame where
scalar replacement and LICM did not fire, those regions do not exist and no word
can land in one.** A zero from a region with no extent says nothing about the
region; it says the optimisation did not run.

So §8's two surviving candidates are **untested on this workload, not
eliminated**, and the prerequisite for testing them is an engagement census —
does scalar replacement or LICM hoisting produce any slots at all in the netty
methods live at the fault? `moving-young-corruption-rootcause.md` hit the same
wall from the other side: its `BinTreesClassic.bottomUpTree` frame was
`FrameLayout { scalar_lo: 0, scalar_hi: 0, ref_hoist_lo: 0, ref_hoist_hi: 0 }`,
which is why that page's §3 verdict was right in shape and wrong in mechanism
for that benchmark.

**What `unclassified` actually is.** Reading the ladder rather than guessing at
it: the final `else if self.reg_spill_hi > 0 && off >= self.reg_spill_hi`
catches everything past the safepoint spill, so `unclassified` is only reachable
when **`reg_spill_hi == 0`** — a frame `x64/frames.rs` built with
`reg_spill_base == 0 || !safepoint_reg_spill`, i.e. one with no safepoint
register spill at all — at an offset past the spill area. 21 such words across 8
reps, in three methods:

```
io/netty/channel/DefaultChannelPromise.setSuccess:()L…;                 off=448
io/netty/channel/ChannelInitializer.initChannel:(L…;)Z                  off=528
io/netty/channel/AbstractChannelHandlerContext.findContextInbound:(I)L…; off=544
```

That is a real and much narrower question — *why does a live compiled frame have
no safepoint register spill, and what is in its tail?* — but it is a different
question from §8's, and it should not be filed under §8's heading.

**Method note.** This is the same engagement trap the rest of this page is
careful about, one level further down: §10.8 read a zero from a census without
first asking whether the thing being counted could occur. The check is one
question — *does this region have a non-empty extent in the frames I am
measuring?* — and it costs nothing to ask before the count is believed.

### 10.10 §7's "the ledger never accepts" is STALE — and that makes §8 testable for the first time

§7 states, and §10.2 repeated: *"on this workload it never accepts: 0
`accounted=true` out of 750 decisions"*, from which §10.2 concluded that every
failure ever measured came from relocating under peers that proved nothing, and
that "the precise maps are incomplete" had therefore never been tested.

**On current dev the ledger accepts, and it is not close.**
`CRATONVM_DBG_XT_COVERAGE=1` under the §10.4 repro, six reps:

```
@@PEERTOTAL proven=144 accounted_true=237 peer_decision_lines=650 crashes=2/6
```

**237 of 650 peer decisions accept.** And they accept on genuine proofs, not on
the pin credit this branch closed — every accepting line has `pinned=0` and
`proven` equal to `peer_depth`:

```
29 x  [xt-coverage] peer_depth=1 proven=1 pinned=0 pins_honoured=false accounted=true
 7 x  [xt-coverage] peer_depth=2 proven=2 pinned=0 pins_honoured=false accounted=true
 3 x  [xt-coverage] peer_depth=3 proven=3 pinned=0 pins_honoured=false accounted=true
 1 x  [xt-coverage] peer_depth=4 proven=4 pinned=0 pins_honoured=false accounted=true
```

**And the accepting cycles are the relocating cycles.** Across two four-rep
arms, `accounted_true` tracks `moving-jit-coverage-proven` almost exactly —
205 vs 207, and 148 vs 154. The cycles that relocate are the cycles whose peers
deposited a proof.

So the configuration that crashes is: **peers proved their own frames
rewritable, the collector relocated on that proof, and the heap was corrupted
anyway.** That is §8's hypothesis with an arm under it, and it removes the
confound §10.2 raised. Precise-only under-coverage is now the live reading, and
for the first time it is being tested rather than bypassed.

**Two readings that are NOT safe to take from the numbers above.**

* `proven=0` on the two crashed reps is an **artifact**, not a zero: a SIGSEGV
  skips the exit trailer, and those logs contain zero `[GC]` lines at all.
  Only the four completed reps contribute a `proven` count.
* Why §7's number was 0 and this one is 237 is **not established**. My
  hypothesis was the A5 residue filter — it screens a false positive in
  `refresh_moving_young_coverage_for_current_thread`, which is the very function
  each peer runs in `publish_peer_jit_coverage_for_stw` before depositing, so
  fixing the peer's own proof should make peers start depositing. **Refuted on
  its own kill switch:** `CRATONVM_JIT_A5_RESIDUE_FILTER=0` gives
  `accounted_true=148/378` against `205/508` with it on — 39 % vs 40 %, no
  effect. Something else between §7's binary and dev opened the ledger, and this
  page should not guess at it a second time.

**What this changes about where to look.** The stale reference is held by a
frame whose peer proof SUCCEEDED. The remap covers the oop-map slots and the
callee-saved GPR image; §10.8 measured that rewriting the entire remaining
unverifiable tail changes nothing. So the surviving candidates are not frame-band
memory at all — they are the channels the proof asserts and the remap does not
walk: a resumed register that is reloaded from somewhere other than the
callee-saved image, or the shadow stack. That is the next measurement, and it is
a different one from §8's screens.
