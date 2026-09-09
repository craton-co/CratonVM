# 19 netty classes fail on **Generational only** — the blocked-region wake never remapped compiled state — FIXED

## Status

**FIXED 2026-09-08.** Root cause: a thread that blocked in a native with
COMPILED frames below it resumed with every JIT-frame oop, register-image word
and shadow-stack entry still at its pre-move address. The wake remapped
interpreter frames and thread-local `ObjectRef`s and nothing compiled.

The fix is one call, `apply_blocked_wake_jit_remap`, added to the ordinary wake
path `NativeContextImpl::check_post_block_gc_refs` and shared with the
leaked-region fallback. Default-ON; `CRATONVM_NO_BLOCKED_WAKE_JIT_REMAP=1` is
the kill switch and the positive control.

| | |
|---|---|
| **Fix** | `vm/src/vm/vm_exec.rs` — `apply_blocked_wake_jit_remap`, called from `check_post_block_gc_refs` |
| **Headline A/B** | one binary, `DuplicatedByteBufTest`: **0/10 SIGSEGV with the remap, 6/6 with its kill switch**; dev tip before the fix **10/10** |
| **Family** | 19 classes at forced engagement: **18/19 classes crashed on dev tip, 0/38 reps with the fix** where no unsafe flag is used |
| **Engagement** | 15–663 relocating cycles a run, against the **9–24** the family originally failed under |
| **Regression suite** | 92/92 |

The predecessor page — *"19 netty classes fail on Generational only — the moving
young cycle corrupts a live object"*, filed 2026-09-06 under
`docs/known-issues/netty/` and retired into this one — is in this file's git
history (`git log --follow --diff-filter=D -- 'docs/known-issues/netty/bytebuf-*'`).
Its §§1–12 narrowed the defect correctly — *relocation is necessary, the stale
reference is outside the heap, it is held by a peer* — and stopped one step
short of the channel. Section numbers below with a `§` refer to that page.

---

## 1. What was actually wrong

`GcBarrier`'s blocked-region protocol has two wake paths:

* `NativeContextImpl::check_post_block_gc_refs` — **the ordinary one**, taken by
  every blocking native (monitor enter, `Object.wait`, `Thread.join`,
  `LockSupport.park`, blocking I/O);
* `apply_pending_blocked_fixups` — the fallback, taken only when a blocked
  region leaked and the next safepoint publish has to heal it.

The JIT half of the wake — `remap_active_jit_frames`,
`remap_register_image_words`, `shadow_stack.remap` — existed, but it was wired
into **the fallback only**, and gated behind an opt-in flag
(`CRATONVM_BLOCKED_WAKE_JIT_REMAP`) that was off by default. So on every
ordinary wake:

1. a peer blocks in a native, with compiled frames below it holding live oops;
2. a young collection relocates those objects and folds its pointer map into the
   peer's `GcBlockState::fixup` chain;
3. the peer wakes and applies the chain to its **interpreter** frames, `printed`,
   handle slots, JNI locals, deopt stashes and native pin roots;
4. …and re-enters compiled code whose frame slots, register images and shadow
   stack still name from-space.

`ThreadExecState::NativeBlocked`'s `RelocationRule` is `PermittedWithRewrite`,
and its comment justified that with "`fold_pointer_map_into_blocked` remaps the
deposited snapshot and composes `fixup` / `slot_origins`; the thread applies them
in `check_post_block_gc`." That was a true statement about half of the state.

`apply_pointer_map_to_thread` — the STW-resume path — has carried exactly these
three calls for the thread that PARKED at the barrier since the day its own
comment was written: *"this stranded a non-initiator's JIT-frame oops at their
old addresses after a relocation — a use-after-free."* Blocked peers were the
same bug one path over.

**The omission was named in the tree before it was found here.**
`xt_jit_coverage_assume`'s doc says the peers that refuse the cycle are "threads
blocked in a native with compiled frames below them… Giving them a deposit means
teaching the blocked-region WAKE to remap JIT frames (it currently remaps only
interpreter frames), which is a real change; this says whether it is worth
making." It was worth making.

## 2. The engagement lever — the question §0.3 and §15 left open

The predecessor page could not close because it could not make the collector
relocate on demand. §0.3 tested five hypotheses (workload concurrency, the
post-evacuation verifier, machine load from a concurrent build, heap size at
1500m and 2g) and refuted all five; §15 then measured the family at 0–3.5
relocating cycles a run and said plainly that the clean result was therefore
weak.

**The driver is the young-collection TRIGGER, and it has a flag.**
`CRATONVM_GC_YOUNG_TRIGGER_PERCENT` overrides the 50 %-of-from-space threshold;
at `=1` a 1 GB heap collects after ~2.5 MB of allocation instead of ~128 MB.
`DuplicatedByteBufTest`, one binary, `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`:

| arm | moving cycles | non-moving |
|---|---:|---:|
| default trigger (50) | 0 | 17 |
| `…TRIGGER_PERCENT=10` | 8 | 75 |
| `…TRIGGER_PERCENT=1` | 14 | 150 |

Why §0.3's five hypotheses all failed is now plain, and it is the same lesson
that section drew about itself: **it varied the MACHINE while the thing gating
relocation was total collection count, which is a heap-sizing policy inside the
collector.** §0.3 also only ever tested heaps LARGER than the default, which
moves the count the wrong way.

With the trigger at 1, `CRATONVM_XT_JIT_COVERAGE_ASSUME=1` amplifies again.
§10.2 reported it dead, correctly, because `unrewritable_conservative_jit_roots`
sits downstream of the peer ledger and refuses the cycle one term later; with
`CRATONVM_GC_NO_PEER_PIN_DIVERT=1` removing that term, ASSUME recovers its effect
exactly as §10.2's own mechanism predicts.

## 3. The A/B, one binary per arm

`io.netty.buffer.DuplicatedByteBufTest`, `--Xmx 1g -XX:+UseGenerationalGC`,
`CRATONVM_GC_NO_PEER_PIN_DIVERT=1 CRATONVM_GC_YOUNG_TRIGGER_PERCENT=1
CRATONVM_XT_JIT_COVERAGE_ASSUME=1`:

| arm | SIGSEGV | tests |
|---|---:|---|
| dev tip `a3469bf00` | **10 / 10** | — |
| dev tip + `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP=1` (§13's kill switch) | **6 / 6** | — |
| dev tip + the capture-lifetime repair alone (§5.1 below) | **4 / 4** | — |
| **with `apply_blocked_wake_jit_remap`** | **0 / 10** | `ok=416 failed=0` |
| …and its kill switch, `CRATONVM_NO_BLOCKED_WAKE_JIT_REMAP=1` | **6 / 6** | — |

The kill-switch row is the control the predecessor page never obtained: §13.4
records its own A/B as void because the crash had stopped reproducing on both
arms. Here the same binary crashes 6/6 with the repair disabled and 0/10 with it
enabled, at 62–372 relocating cycles a run.

The fault is the one §10.4 and §11.1 describe, on both platforms:

```
#  SIGSEGV at pc=0x7c3869cdee20, addr=0x7c387c82691e
#  fault addr is inside a RECENTLY DECOMMITTED heap span: base=0x7c387c600000 site=unbumped-middle
#  fault pc is inside a LIVE registered code buffer: base=0x7c3869cdee000
```

Compiled code dereferencing a stale pointer into evacuated space.

## 4. The family, at engagement an order of magnitude above what it failed under

All 19 classes of §1, `--Xmx 1g -XX:+UseGenerationalGC`, with the divert removed
and the trigger at 1.

**Without the unsafe `ASSUME` flag** (`CRATONVM_GC_NO_PEER_PIN_DIVERT=1
CRATONVM_GC_YOUNG_TRIGGER_PERCENT=1`), 2 reps of each class:

| binary | reps | crashes |
|---|---:|---:|
| dev tip | 38 | 1 (`AdvancedLeakAwareCompositeByteBufTest`) |
| **with the fix** | 38 | **0** |

**With `ASSUME=1` on top**, which forces the peer ledger to accept:

| binary | classes crashing |
|---|---:|
| dev tip, 1 rep each | **18 of 19** |
| **with the fix**, 2 reps each | **2 of 19** (`AdvancedLeakAwareByteBufTest`, `AdvancedLeakAwareCompositeByteBufTest`) |

Every completing run reports `failed=0`, and **`NullPointerException` does not
appear**: the `verifyInvokedAtLeastOnce` NPE that opened the page is gone at
15–663 relocating cycles a run, against the 9–24 §4 recorded when the family was
failing. Representative rows: `LittleEndianHeapByteBufTest` 325 and 429 moving
cycles, `ok=412 failed=0`; `PooledBigEndianHeapByteBufTest` 663 moving cycles,
`ok=417 failed=0`; `UniqueIpFilterTest` 204–345 moving cycles, 0/10 crashes;
`BigEndianCompositeByteBufTest` 140 and 144, `ok=487 failed=0`.

`NioEventLoopTest` reports `ok=13 failed=0` and then does not exit — the
fixture's non-daemon event loop, which HotSpot does identically (§10.6).

### 4.1 What is NOT fixed — **superseded by §16, which root-causes it**

`AdvancedLeakAwareByteBufTest` and `AdvancedLeakAwareCompositeByteBufTest` still
crash **when `CRATONVM_XT_JIT_COVERAGE_ASSUME=1` is set**.

> **§16 supersedes this section's reading.** This section attributed the
> residual to "the residue ASSUME is designed to ignore" — cycles relocating
> under frames whose oop maps are genuinely incomplete, i.e. to an operator
> overriding a correct refusal. **That is wrong.** §16 root-causes it to a
> specific JIT operand-stack oop-marking gap in two named netty methods, on a
> thread that HAD applied the collection's pointer map. ASSUME does not cause
> the defect; it removes a coincidental refusal that was hiding it. The
> fallback-reason histograms this section quoted are real, and are not evidence
> for the claim they were used to support.

Two further honest limits:

* the two classes were also each seen to crash once **without** `ASSUME`, on BOTH
  binaries, early in this work (1/5 and 2/5). A 12-rep A/B afterwards read
  **0/10 on both**, and the 38-rep family arm above read 0 on the fix and 1 on
  dev tip. That is a rare event neither arm separates, and it is recorded as
  unattributed rather than as a fix or as a regression;
* everything in §4 requires `CRATONVM_GC_NO_PEER_PIN_DIVERT=1`. At the shipped
  default `unrewritable_conservative_jit_roots` refuses these cycles and the
  family has always been green. This repair is what makes RESTORING moving-young
  engagement possible; it is not what makes the shipped configuration safe,
  which it already was.

## 5. Four secondary repairs found on the way

### 5.1 The 2026-09-07 blocked-peer stack remap had no per-cycle lifetime

`gc_quiescence::clear_peer_stack_slots` shipped with §13's repair and **had no
caller**, and the drain on the other side
(`fold_pointer_map_into_blocked_audited`) is reached only through
`update_all_roots`, which returns early on an empty pointer map — that is, on
every NON-moving cycle. Non-moving cycles outnumber moving ones by roughly forty
to one on this workload, so the capture buffer was in practice a
process-lifetime accumulator. Two consequences, both correctness ones:

* **the cap silences the repair.** `record_peer_stack_slot` drops a capture once
  the buffer holds 65536; once the accumulation saturates, the moving cycle —
  the only cycle whose captures matter — records nothing.
* **ABA.** A capture taken at cycle N carries `orig` = the word's value *then*.
  Folded at a later cycle M it is advanced through M's pointer map, so if the
  address was vacated at N, recycled, and moved again at M, the wake write-back
  stores the NEW occupant's address into a word that meant the old one — the
  repair manufacturing exactly the wrong-address read it exists to prevent.

Fixed by calling `clear_peer_stack_slots` (and `clear_peer_reg_capture`, which
has the same shape and the same drain) from
`begin_moving_young_coverage_cycle`, the point that already resets
`CONSERVATIVE_JIT_SCANS` — the very counter whose scans produce these captures.

The census line gained three fields so the difference is visible: `discarded` (a
correct discard — the cycle that captured never relocated), `dropped` (the buffer
at its cap, a repair OUTAGE), and `unrouted` (a capture on a RELOCATING cycle
that no blocked thread claimed — a word nothing will ever rewrite). Measured on
this workload: `dropped=0` and `unrouted=0` on essentially every run, and
`written` unchanged at ~2 800–3 000 words a run, so the per-cycle lifetime
removes the hazard without costing coverage.

`unrouted` is also the standing test for the take-over path. A peer frozen as
`CompiledUninterruptible` is not in a blocked region and has no wake hook, so its
captures would be unroutable; the counter reads 0 here because `xt_peer_scan`
reports `taken_over=0` on this workload, and it will report non-zero the moment
that stops being true.

### 5.2 The moving-young heap verifier reported abandoned CAS-loser copies

`CRATONVM_MOVING_YOUNG_VERIFY=1`'s contract is *"non-zero = a HEAP rewrite was
missed; zero with a wrong result = the stale reference lives OUTSIDE the heap"*,
and §6 built a whole (later withdrawn) root cause on two non-zero readings whose
shape was *"an object whose every slot was missed"*.

**Those are abandoned CAS losers.** `ParEvac::evacuate` copies first and claims
second; a worker that loses the forwarding CAS has already written a complete
object at its own destination and simply walks away from it — "to-space garbage
reclaimed next cycle". Nothing references it, so nothing scans it, so every one
of its reference slots keeps the pre-move address. That explains both halves of
the signature: whole-object, because the object was never scanned; and harmless,
because it is unreachable garbage no mutator can dereference.

Reproduced here on `AdvancedLeakAwareByteBufTest`: `young=5 old=0` on one cycle
with all five slots belonging to one `LinkedHashMap$Entry`, and `young=0 old=5`
on another with all five belonging to one `TestMethodTestDescriptor`.

The loser arm now records its abandoned destination (only while the verifier is
armed) and the verifier skips it, reporting
`abandoned_cas_loser_copies_skipped=N` beside its counts. After the change the
verifier reads `young=0 old=0` on the same workload, including on crashing reps
— which restores §6's dichotomy as a usable instrument and confirms its
conclusion for the right reason.

### 5.3 `relocation_blockers()` could not state the obligation it documented

§10.11 found that `ThreadStateCensus::relocation_blockers()` — whose doc calls a
non-zero answer "the shadow-side statement of the
`mark_moving_young_coverage_incomplete_because` obligation" — reads `blockers=1`
on every collector decision, relocating or not, because `JavaRunning` and
`VmRunning` are both `Forbidden` and the collecting thread is in one of them by
construction.

`ThreadStateCensus::peer_relocation_blockers(initiator)` is the usable form. It
ANDs `RelocationRule::Forbidden` with `may_hold_unrewritable_object_refs` — so a
thread merely RUNNING, which the safepoint is about to park through a path that
rewrites it, no longer counts — and subtracts the initiator, so a zero is
reachable at all. `CRATONVM_DBG_RELOCATION_BLOCKERS` prints both, and unit tests
cover the subtraction and the running-threads-only case.

### 5.4 A leaked blocked-region exit could strand a native-slot fixup

The safepoint self-heal in `gc_and_alloc.rs` tested `fixup` and `slot_origins`
for pending work and not `native_slots`, so a thread whose only pending repair
was a raw stack word ran on with it. Now tested.

## 6. Corrections to the predecessor page

* **§0.3's "the variance is UNEXPLAINED" — explained.** The driver is
  `CRATONVM_GC_YOUNG_TRIGGER_PERCENT`; see §2 above. The five refuted hypotheses
  stay refuted.
* **§6's withdrawn root cause was withdrawn for the right reason and rested on
  misread evidence.** The two non-zero verifier readings were abandoned CAS
  losers, not a scan cursor finishing early; see §5.2.
* **§7's amplifier table and §10.2's "ASSUME no longer amplifies" are both
  correct as measured and both conditional on the divert.** With
  `CRATONVM_GC_NO_PEER_PIN_DIVERT=1` set, ASSUME amplifies again.
* **§8/§10.9's scalar-replacement and LICM-hoist candidates are now countable.**
  `FrameLayout::candidate_region_extents` and the
  `[GC] jit_frame_region_extents` census answer the prerequisite question §10.9
  identified — *does this region have a non-empty extent in the frames being
  measured?* — so a per-region zero can no longer be read as evidence without
  its denominator.
* **§13's repair is retained and is now correct per cycle** (§5.1). It remains
  justified by the direct measurement it always had — words that named relocated
  objects now name the right ones — and NOT by crash elimination: its kill
  switch reads 6/6 crashes either way on this repro, which is how §3 attributes
  the fix to the wake remap instead.
* **§14's withdrawal stands.** `CRATONVM_GC_HOLD_HELPER_PEERS` and
  `CRATONVM_GC_REMAP_FROZEN_PEER_STACKS` are gone and are not needed: the
  channel they reached is reached by the wake instead, with no peer held across
  a copy.
* **§15's "what cannot be said" can now be said for 17 of 19 classes**, and §4.1
  says exactly which two it still cannot be said for and under which flag.

## 7. Reproducing (for anyone re-opening this)

```bash
CV=<cratonvm>
JDK=/data/toolchain/jdk-25
ARGS=/data/cratonvm/apps/netty-suite-runner/common.args

# 6/6 SIGSEGV with the repair disabled
CRATONVM_GC_NO_PEER_PIN_DIVERT=1 \
CRATONVM_GC_YOUNG_TRIGGER_PERCENT=1 \
CRATONVM_XT_JIT_COVERAGE_ASSUME=1 \
CRATONVM_NO_BLOCKED_WAKE_JIT_REMAP=1 \
  "$CV" --java-home "$JDK" --Xmx 1g -XX:+UseGenerationalGC "@$ARGS" \
        -Dcraton.batch=1 CratonRunner io.netty.buffer.DuplicatedByteBufTest

# the same command without that last variable: ok=416 failed=0,
# 62-372 relocating cycles, 0/10 crashes
```

`CRATONVM_GC_STATS=1` reports
`[GC] blocked_peer_stack_remap: … discarded= dropped= unrouted=` and
`[GC] jit_frame_region_extents:`. `CRATONVM_DBG_PEER_REG_PAIRING=1` reports
`[peer-reg-stale] cycle summary: … stale= interior= dead_base=`, where
`interior` counts peer stack words naming the INTERIOR of a relocated object —
the derived-pointer population that no conservative write-back can repair
(15 230 of them in one clean `DuplicatedByteBufTest` run), and the standing
reason `unrewritable_conservative_jit_roots` remains the shipped default.

## 16. The `AdvancedLeakAware*` residual, root-caused — an operand-stack oop the JIT never marked

§4.1 filed the last two crashing classes as "the residue `ASSUME` is designed to
ignore". That is wrong, and this section replaces it. The residual is a JIT
codegen defect with a named method, a named slot and a named instruction; the
only thing `ASSUME` contributes is removing an unrelated refusal that was
hiding it.

### 16.1 A reproducer that is not intermittent

`io.netty.buffer.AdvancedLeakAwareByteBufTest`, `--Xmx 1g
-XX:+UseGenerationalGC`, `CRATONVM_GC_NO_PEER_PIN_DIVERT=1
CRATONVM_GC_YOUNG_TRIGGER_PERCENT=1 CRATONVM_XT_JIT_COVERAGE_ASSUME=1`:
**SIGSEGV 8/8**, and 5/5, 4/4, 3/3 on every later batch. §4.1's "1/5 and 2/5,
then 0/10" was the same defect measured at 2 reps a class on a loaded host, not
a rare event.

`ASSUME` is REQUIRED and is the only flag that is: **5/5 with it, 0/5 without**,
both at the same trigger and the same divert removal.

### 16.2 One method, one instruction, every time

`CRATONVM_DBG_JIT_NAMES=1` names the faulting body in every crash:

```
#  jit pc  : io/netty/buffer/SimpleLeakAwareByteBuf.newSharedLeakAwareByteBuf(…)
```

and the fault pc is at the **same code offset, 0x216, in every run**.
`CRATONVM_DBG_JIT_DISASM` gives the instruction and the two before it:

```
 213: 498bc5             mov  rax,r13          ; rax = this
 216: 8b880f000000       mov  ecx,[rax+0Fh]    ; <-- FAULTS
 21c: 4883e104           and  rcx,4
```

`[rax + 15]` is the object header's `GC_FLAGS` byte and `4` is
`GC_FLAG_COMPACT`: this is the compact-`getfield` arm's receiver dereference,
the method's FIRST touch of `this`. The prologue disassembly shows `mov r13,rsi`
— `r13` is `this`, arriving in the argument register.

**The receiver is already stale when the method is entered.** That is why
`CRATONVM_JIT_DENY` on this one method is **0/4 against 4/4** (the interpreter
reads the receiver from a remapped frame slot instead), and why denying either
callee changes nothing (4/4).

### 16.3 The stale value, proved by the collector's own ledger

`CRATONVM_DBG_VACATED_FRAMES=1` plus the signal-safe register scan added with
this section (`crash_handler`, "VACATED REGISTER") reports, on every crash:

```
#  VACATED REGISTER: rax=0x740ab02cb370 named an object a completed collection moved to 0x740ac0200018
#  VACATED REGISTER: rsi=0x740ab02cb370 …
#  VACATED REGISTER: r13=0x740ab02cb370 …
#  this thread last applied a relocation map at cycle=0x12 path=1 of relocating_cycles=0x12
```

Two facts, and the second is the one that matters:

* the value is not "an address that looks moved" — it is a key of the
  collector's vacated ledger, which subtracts that cycle's destinations;
* **`cycle == relocating_cycles` and `path=1`**: this thread applied the pointer
  map for the most recent relocating cycle, through the STOP-THE-WORLD RESUME
  (`apply_pointer_map_to_thread`, which runs `remap_active_jit_frames`,
  `remap_register_image_words` and the shadow-stack remap). This is **not** a
  thread that never got the map. The rewrite ran and the reference is stale
  anyway.

### 16.4 The unpublished slot

`CRATONVM_DBG_SHADOW2=1 CRATONVM_DBG_SHADOW2_FILTER=LeakAwareByteBuf` prints,
per safepoint, the simulated operand stack, its oop marks, and the homes
`collect_live_oop_homes` publishes on the shadow stack. Across the whole
leak-aware family there are exactly **two** operand entries marked `false`, and
they are in exactly the two callers of the faulting method:

```
AdvancedLeakAwareByteBuf.duplicate:  pc=8  stack=[Frame(104)] marks=[false]  homes=[Reg(12), Frame(64)]
AdvancedLeakAwareByteBuf.slice:(II)  pc=10 stack=[Frame(136)] marks=[false]  homes=[Reg(14), Frame(80)]
```

`Frame(104)` and `Frame(136)` are **absent from `homes`** —
`collect_live_oop_homes` skips any entry whose mark is `false`. Contrast the
sibling that behaves:

```
SimpleLeakAwareByteBuf.<init>: pc=20 stack=[Frame(80), Frame(88)] marks=[true, true]
                               homes=[Frame(80), Frame(88), Reg(15), …]
```

So the chain, end to end:

1. the caller pushes the receiver for the pending
   `newSharedLeakAwareByteBuf(...)` call and then makes a GC-capable call
   (`super.duplicate()` / `super.slice()`);
2. that operand entry lives in a frame slot and the JIT's per-slot oop tracker
   has it marked **not a reference**, so it is published on **no rewritable
   channel** — not the shadow stack, and not the map the shadow stack's
   coverage bit is derived from;
3. a moving young collection during the inner call relocates the receiver; the
   frame slot keeps the pre-move address;
4. the slot is popped and passed in `rsi` to `newSharedLeakAwareByteBuf`;
5. its first receiver dereference — `mov ecx,[rax+0Fh]` — reads decommitted
   from-space → SIGSEGV.

**The structural hazard behind it**: `Compiler::push_stack` pushes
`stack_oop_marks.push(false)` and relies on the opcode handler calling
`mark_top_as_oop()` afterwards. The default is the UNSAFE direction — a missed
call does not produce a conservative over-approximation, it silently drops a
live oop off every rewritable channel. `stack_push(slot, is_oop)` takes the bit
explicitly and is the safe shape; the `push_from_rax()` + `mark_top_as_oop()`
pair is the one that can be broken by omission. Which emitter path drops the
mark for these two shapes is the remaining unknown — the `aload_0..3`,
wide-`aload`, `canonicalize_stack` and `flush_scratch_registers` paths were all
read and all preserve the mark correctly.

### 16.5 The SIGSEGV and this page's original NPE are the same defect

`CRATONVM_GC_RESERVE=0` keeps the vacated granules mapped instead of
decommitting them. On the same repro:

| arm | crashes | result |
|---|---:|---|
| baseline | **5 / 5** | — |
| `CRATONVM_GC_RESERVE=0` | **0 / 5** | `ok=424 failed=2`, 49 moving cycles |

The crash disappears and **two tests fail instead**. That is the same stale
receiver, read rather than faulted on — and it closes the loop with §2 of the
predecessor page, whose symptom was never a crash but a
`NullPointerException` on a live object inside JUnit's `ValidatingInvocation`.
The faulting instruction is additionally a registered **implicit-null-check**
site (the compact-`getfield` arm's `GC_FLAGS` read is what
`bind_implicit_null_recovery` binds), so a stale receiver whose address the
recovery accepts is reported as an NPE rather than as a fatal signal. One
defect, three faces, selected by whether the vacated span is mapped and by where
the stale address lands.

### 16.6 Arms that proved nothing, recorded so they are not re-run

Several of these were run and read before they were checked. They are listed
because this page's whole history is made of exactly that mistake.

* **`CRATONVM_NO_SHADOW_STACK=1` — the variable does not exist.** The real
  token is `CRATONVM_SHADOW_STACK` (opt-in). The 3/3 that arm reported is a
  measurement of nothing.
* **`CRATONVM_SHADOW_PIN=1` (4/4)** publishes shadow oops as PINNED roots — and
  `VmHeap::Generational::honours_conservative_pins()` is `false`, so a Cheney
  copy moves them anyway. Vacuous on this collector by construction.
* **`CRATONVM_SHADOW_NOPUSH=1` (0/4)** looks like a fix and is not: it also sets
  `pending_shadow_coverage_complete = false`, which makes every safepoint of the
  method report incomplete coverage and DIVERTS the cycles this frame is live
  for. It suppresses the relocation rather than surviving it.
  **`CRATONVM_SHADOW_NORELOAD=1` is the arm that isolates the reload** — it
  keeps the push, so coverage and engagement are unchanged — and it reads
  **5/5**. The shadow reload is innocent.
* **The JIT-frame vacated auditor added with this section over-reports.** It
  walks the frame band conservatively with no per-bci liveness filter, so a DEAD
  java-local slot holding a vacated address counts as a hit: the non-crashing
  no-`ASSUME` control read 5136 "verifiable" hits against the crashing arm's
  320. Its per-region tally is usable; its totals are not a signal.
* **Refuted hypotheses**, each with an arm: nested inlining
  (`CRATONVM_JIT_INLINE_NEST=0` → 4/4), the callees
  (`CRATONVM_JIT_DENY` on either `newLeakAwareByteBuf` → 4/4), the existing
  remap widenings (`CRATONVM_JIT_REMAP_ALL_UNVERIFIABLE`,
  `CRATONVM_GC_NO_BLOCKED_PEER_STACK_REMAP`,
  `CRATONVM_NO_BLOCKED_WAKE_JIT_REMAP` → all 3/3 or 4/4), the entry poll's
  sign-extended sp-id (`active_safepoint_id` truncates with `as u32`, which
  recovers `ENTRY_POLL_BC_PC` exactly), and **publishing the flushed frame slot
  alongside the register home** for every register-resident oop local — built,
  measured **3/3**, and reverted.
* `CRATONVM_JIT_SAFEPOINT_POLLS=0` reads **1/4 with MORE relocation** (64 moving
  cycles). Suggestive that the parking point matters, not conclusive, and not
  built on here.

### 16.7 What a fix has to do, and what it must not

The repair is to mark that operand entry as a reference so
`collect_live_oop_homes` publishes it — not to widen a remap. Every remap-side
lever above is null because the value is on no channel to remap.

Two guards for whoever takes it:

* **do not "fix" it by defaulting `push_stack`'s mark to `true`.** A false
  `true` pins a non-reference word and, worse, hands a non-address to the shadow
  reload to store into a live home. The bit has to be right, not conservative.
* **`CRATONVM_SHADOW_NOPUSH`, and any arm that changes
  `pending_shadow_coverage_complete`, is not a control** — it changes whether
  the cycle relocates at all. Use `CRATONVM_SHADOW_NORELOAD`,
  `CRATONVM_JIT_DENY`, or `CRATONVM_GC_RESERVE=0`, all of which leave engagement
  intact.

### 16.8 Scope

Unchanged from §4.1's surviving half: this is reachable only with
`CRATONVM_GC_NO_PEER_PIN_DIVERT=1` **and** `CRATONVM_XT_JIT_COVERAGE_ASSUME=1`.
At the shipped default these two classes are 0/4, and with the trigger alone
0/4, because `unrewritable_conservative_jit_roots` refuses the cycles. It is a
blocker for restoring moving-young engagement, not for the shipped
configuration.
