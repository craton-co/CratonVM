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

### 4.1 What is NOT fixed, stated precisely

`AdvancedLeakAwareByteBufTest` and `AdvancedLeakAwareCompositeByteBufTest` still
crash **when `CRATONVM_XT_JIT_COVERAGE_ASSUME=1` is set**. That flag's own doc
calls it "A MEASUREMENT INSTRUMENT, and unsafe to run with… A peer that
deposited nothing has NOT proved its frames rewritable, and relocating under it
strands its oops." Its stated purpose was to price this very repair; with the
repair in place it takes the family from 18/19 to 2/19, and what remains is the
residue it is designed to ignore — cycles relocating under frames whose oop maps
are genuinely incomplete. `compiled-frame-oop-not-published`,
`innermost-rbp-belongs-to-unguarded-callee` and
`xt-helper-window-conservative-scan` all appear in those runs' fallback
histograms. **No wake-time rewrite can repair an oop that no map names**, and
that population is refused by default: at the shipped default these two classes
are 0/4, and with the trigger alone (no divert removal) 0/4.

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
