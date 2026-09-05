# The box/unbox intrinsic SIGSEGVs under a relocating collector

## ROOT CAUSE FOUND 2026-09-04 -- it is not a stale root at all

The compaction SLIDE writes into a granule the arena DECOMMITTED.

`relocate_stw` picks a slide destination and copies a survivor into it on a
SAFETY argument that says `to` "is inside the arena and strictly below `from`".
Inside the arena is NOT committed: the arena reserves address space and commits
granules on demand, and `decommit_unbumped_middle` / `decommit_free_blocks` hand
granules back to the OS while their addresses stay reserved. The destination
search screens by page and by liveness and never by COMMIT STATE.

Observed, not inferred. A fault-time witness (`gc::reloc_witness`, a granule
bitmap read from the signal handler) reports on every crash:

| | Z-bm1 | Z-bm4 |
|---|---|---|
| faulting access | **write** at `0x2B1F44A0000` | **write** at `0x23EF57A0000` |
| decommitted span | `[0x2B1F44A0000, 0x2B1F46A0000)` | `[0x23EF57A0000, 0x23EF59A0000)` |
| decommitted by cycle | 15 | 21 |
| **offset into span** | **0x0** | **0x0** |
| frame | `relocate_stw+0x2ECE` | same |

Four facts, each killing a class of explanation: it is a **WRITE** (every repair
attempted assumed a stale reference being DEREFERENCED); the offset is **0x0**,
the granule BASE, where no object pointer lands twice by chance; the span is
exactly one 2 MiB `reservation::GRANULE`, which is commit geometry and not heap
geometry; and the frame is the slide's own `ptr::copy`, not compiled code.

**So this page's framing was wrong.** "A reference held in a live JIT frame that
relocation moved without rewriting, a root the safepoint's oop map does not
name" describes no part of this. No stale root is involved.

That also explains why `RELOCATE_UNDER_PROVEN_JIT=0` and the box/unbox intrinsic
both looked causal. Neither is: both change how much COMPACTION happens, and
compaction is what runs slides. The intrinsic's speedup raises allocation
pressure, the switch removes relocation outright -- each moves the number of
slides, which moves the chance of landing in a decommitted granule.

### The fix, and its cost

`Arena::ensure_committed_span` commits the destination before the copy; a commit
that fails leaves the object where it is. Measured on
`org.h2.test.jdbc.TestCachedQueryResults`, 5 runs:

| | before | after |
|---|---|---|
| SIGSEGV | 2-3 of 4 | **0 of 5** |
| ref-array OOM | 1497 | **0** |
| `actual` | 98304 | **99953-99978** |
| completes | ~1519 s | 555-728 s |
| compaction | -- | 25 cycles, 545893 objects |

Regression suite 88/88. It costs nothing because it COMMITS memory rather than
refusing to relocate -- unlike every guard measured on this family, which bought
safety at 2570-14514 fragmentation OOMs and total loss of completion.

Same shape as the `gen_evac` parallel-copy fault fixed 2026-09-02: a path that
bypasses `Arena::hand_out` commits nothing.

### Method note, worth more than the fix

Seven repairs were proposed, implemented and measured before this, all aimed at
"which reference went stale" -- unnamed frame slots, duplicate homes, unreached
local masks, blocked-peer remap, misaligned interiors, one-past-the-end cursors,
and two blanket refusals. None could work, because the category was wrong.

Every measurement was consistent with the stale-root framing AND with the truth,
so nothing forced the question. What broke it was an instrument that reports
FACTS rather than adjudicating a hypothesis -- and the three facts that settled
it (write, offset 0x0, `relocate_stw`'s own frame) were present in the very
first crash dump.

**When repeated targeted fixes all fail to move a defect, that is evidence the
CATEGORY is wrong, not that the next candidate inside it is closer.**

## Status

**RESOLVED 2026-09-04** — root cause found and fixed (the section above:
`relocate_stw` slid a survivor into a granule the arena had decommitted;
`Arena::ensure_committed_span` commits the destination first). The intrinsic is
back at its shipped default of **ON** since `069e67b43`, and the crash does not
reproduce.

**Everything below this line is the investigation, not the answer.** It is kept
because most of it is refuted hypotheses with the measurements that refuted
them, and because two of those refutations cost days. Read it as history: the
sections dated 2026-09-02 and 2026-09-03 reason from a stale-root framing that
the root-cause section retires outright, and they say so in place.

Three of the things this page left for a next reader are now closed:

| item | state |
|---|---|
| the root cause | fixed 2026-09-04, `ensure_committed_span` |
| the intrinsic's default | back ON, `069e67b43` |
| the blocked-wake JIT remap | landed (`vm_exec::apply_pending_blocked_fixups` now remaps JIT frames and the register image) |
| the `op:1033` blocker on the repro | fixed — it was the guarded-inline native screen asking the declaring class, `internal/fixed-bugs/guarded-inline-native-screen-asked-the-declaring-class-FIXED-20260904.md` |

The one item still open is `local_mask_unreached` — see **What is still open**
at the end.

## What happens

`org.h2.test.store.TestRandomMapOps --Xmx 256m`, shipped default, quiet host:
**SIGSEGV in 25-183 s, 11 runs out of 11.** The fault address is always a page
boundary (`addr=0x...f0000`, `rdi` equal to it, fault pc inside libc) which is
what a read through a reference into a page the collector has already vacated
looks like. The in-process report is truncated by design -- it needs the
allocator, which is not safe in a signal handler -- so the Java frame is not in
it.

## Two switches each remove it

Same binary, three runs of 1200 s per arm, nothing else on the host:

| arm | SIGSEGV |
|---|---|
| shipped default | **11 / 11** (25-183 s) |
| `CRATONVM_ZGC_RELOCATE=0` | 0 / 3 |
| `CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` | 0 / 3 |

So it takes BOTH relocation and this intrinsic. Neither alone is enough, which
is why it did not show up in the perf work that landed the intrinsic.

## The bisect, and why it lands on a merge

`git bisect` over the 200 commits between the last known-good tip
(`777688aa5`) and the crashing one (`120bb7c37`):

* both endpoints were re-verified in the **same build profile** first -- the
  known-good binary had been a release build and the crashing one `livedbg`,
  and a cross-profile comparison is not a bisect endpoint;
* only `SIGSEGV` counted as BAD. The `NullPointerException` and the
  fragmentation `OutOfMemoryError` this workload also produces both PRE-DATE
  the range (see
  `known-issues/h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md`),
  and counting them would have bisected to the wrong defect. One commit in the
  range failed twice with `rc=1` and was still, correctly, scored GOOD.

First bad commit: **`a910b7d9c`** -- a MERGE whose two parents are BOTH good.
Its relocation files are byte-identical to parent 2, so nothing was
hand-resolved there. The defect is the INTERACTION between
`perf/box-random-intrinsics-20260902` and dev's relocation, not either alone.

## Narrowed 2026-09-02 (later): it is relocation UNDER LIVE JIT FRAMES

Four more arms on the pre-fix binary (intrinsic default-ON), 1200 s cap, quiet
host, each with the default arm re-run in the SAME batch as a positive control
so a quiet batch cannot be mistaken for a fix:

| arm | SIGSEGV |
|---|---|
| default (positive control) | **2 / 2** (34 s, 121 s) |
| `CRATONVM_COMPACT_REF_FIELDS=0` | **3 / 3** — hypothesis REFUTED |
| `CRATONVM_ZGC_RELOCATE=0` | 0 / 3 |
| `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` | **0 / 3** |

**Refuted: the compact/legacy offset fallback.** `AtomicLongFieldLayout::new`
falls back to `compact = legacy` when `compact_field_storage` has no entry, and
the emitted code still branches on `GC_FLAG_COMPACT` and uses `compact` for a
compact instance — so a compact object read at a legacy offset can land past
its end, which would fault exactly like this. It is a real smell and it is NOT
this defect: with compact fields off entirely, all three runs still SIGSEGV.
Written down because it looked convincing and cost one run to rule out.

**Confirmed: the narrow switch is enough.** `RELOCATE_UNDER_PROVEN_JIT=0` keeps
compaction on everywhere EXCEPT under live compiled frames, and that alone
removes the crash. So the stale value is a reference held in a LIVE JIT FRAME
that relocation moved without rewriting — a root the safepoint's oop map does
not name.

That is the same family as
`known-issues/h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md`,
which has been hunting an unnamed root in a compiled frame for days and whose
oracle reports `local_oop=0`. This is a fresh, cheap, 100%-reproducible
instance of that shape — and unlike that page's witness, this one has a switch
that turns it on and off in one binary.

## The shape of the suspicion -- and why it cannot be right as stated

`bytecode_walk.rs`, region `BOX_UNBOX`, inlines `Long.longValue()J` and
`Integer.intValue()I`. It pops the receiver off the simulated operand stack and
then dereferences it three times -- the class-id guard at `[RAX]`, the GC-flags
byte, and the payload load -- with no call and therefore no safepoint between
them.

This page originally read: *"That is sound only while the receiver in hand
cannot go stale. Under a moving collector it evidently can."* **That cannot be
the mechanism.** Relocation here is stop-the-world -- `ZgcRealHeap::relocate_stw`
-- so the mutator is parked at a safepoint while objects move. A receiver held
in a register across a stretch containing NO safepoint is not the unsafe case;
it is precisely the safe one. Nothing can move under that sequence.

So the receiver must already be stale when it is LOADED. The question is not
"what moves it while we hold it" but **"why was the slot it came from not
healed at the last relocating safepoint"** -- which is a question about the oop
map, not about the length of the inline sequence.

That also retires the next step this page used to propose. Keeping the receiver
as a named root ACROSS the sequence fixes nothing under STW relocation, because
there is no safepoint inside the sequence for a root to matter at.

## What a targeted probe rules out (2026-09-02)

`probes/BoxUnboxReloc.java` -- a hot compiled unbox of long-lived boxed
receivers, interleaved with garbage so their pages fragment and become
compaction candidates. **8 runs per arm, intrinsic ON and OFF, zero crashes and
zero wrong answers**, with all three ingredients measured as ENGAGED in the same
run:

| ingredient | how it was confirmed |
|---|---|
| the intrinsic | 3 sites claimed (`CRATONVM_DBG_ATOMIC_INTRINSIC=1`) |
| relocation | `objects_relocated=34629`, `compaction_cycles=2` (`CRATONVM_GC_STATS=1`) |
| the bail edge | `nullBails=19200` -- null receivers deopt through reason 6 |

The bail edge is in there deliberately: it is the only CALL anywhere near this
sequence, it is taken with the receiver already popped from the simulated
operand stack, and it was the one remaining place a safepoint could open a
window. It does not.

**Read those counters before believing any result from this probe.** The first
version of it ran clean 3/3 and meant nothing: it allocated the receivers in
one dense block, so `objects_relocated` was 0 and the collector never had a
reason to move them. A relocation defect cannot be exercised by a run that
relocates nothing.

`probes/BoxUnboxRelocMT.java` varies the one thing left -- thread count. Four
mutator threads unboxing the same tables while a fifth fragments them, 30 s per
run, **3 runs per arm, no crash and no wrong answer in any of them**:

| arm | compaction cycles | objects relocated | null bails |
|---|---|---|---|
| intrinsic ON | 173 / 204 / 211 | 3.2M / 3.7M / 3.9M | ~300k |
| intrinsic OFF | 150 / 226 / 281 | 2.7M / 4.1M / 5.0M | ~480k |

Five million relocations with the family enabled, and nothing. So whatever H2
does, it is not simply "unbox a relocating receiver on several threads".

Two extraction traps this cost, both worth avoiding on the next attempt.
`objects_relocated=` appears on more than one line, and grepping the whole log
for the LAST one reports 0 while the summary line says 3.2M -- restrict to
`zgc-features:` first. And at 6 threads the workload stops compacting
altogether (`objects_relocated=0` in every run), so a thread count chosen for
"more pressure" can quietly remove the very ingredient being tested.

The lead that remains is what `TestRandomMapOps` does that this does not:
receivers that are not `Long`/`Integer` from `valueOf`, a deopt from somewhere
other than the null check, or an interaction with the map's own structure.

Note what the sequence does to the receiver's SLOT: `pop_stack()` releases it,
and `push_from_rax()` can hand the same spill slot straight back for the
primitive result. Between those two the reference lives only in `RAX`. Any
safepoint that observes the frame in that window sees a slot the map no longer
names — or, worse, names as holding the primitive that replaced it.

## 2026-09-03: a second trigger, with this intrinsic DISABLED

`org.h2.test.jdbc.TestCachedQueryResults` SIGSEGVs 2 of 3 runs (185 s, 100 s) on
merged dev with the box/unbox intrinsic at its new default -- OFF. The enable
flag appears nowhere in those logs. Details and arms:
`known-issues/h2/bug-h2-testcachedqueryresults-zgc-oom-livelock-20260829.md`.

What makes that workload crash is an experimental change
(`CRATONVM_XT_PINNED_PEER_DEPTH=1` + `CRATONVM_XT_PEER_SHADOW_SCAN=1`) whose
only effect is to let relocation proceed while compiled frames are live:
`relocation_on_proven_jit` 2 -> 22, `relocation_skipped_jit` 877 -> 3,
`objects_relocated=519932`.

So the two ingredients this page names are not both necessary. Relocation under
live compiled frames is sufficient on its own; the intrinsic is one way to reach
the bad root, not the only one. That is evidence FOR this page's own narrowed
conclusion -- "a reference held in a LIVE JIT FRAME that relocation moved
without rewriting, a root the safepoint's oop map does not name" -- and against
any remaining account in which the inline unbox sequence is itself the
mechanism.

It also gives the root-cause hunt a second reproducer on a different workload,
which the surviving run shows is otherwise well-behaved (99978/100000, zero
OOM, zero NPE, 22 compaction cycles).

### The second trigger behaves like this one on every switch, and the oracle is blind to both

`CRATONVM_ZGC_RELOCATE=0` removes it: **0 / 3**, matching this page's own 0/3.
The fault `rdi` is page-aligned in every crash (`0x232ECD30000`,
`0x28DEA7B0000`, `0x1CA01BB0000`) -- this page's signature exactly.

`CRATONVM_DBG_VERIFY_OOP_MAPS=1` does NOT find the root. It refutes
`fully_oop_covered` immediately on both workloads, but reports
`verifier_oop=0 verifier_not_oop=1222475 verifier_unknown=1582066` against a
populated `name_index=(1237 names)`. Every one of 2.8 M never-mapped words is
either confirmed not-a-reference or unknown; none is corroborated. So the
unnamed root is not something this oracle can see as an oop -- which is itself a
constraint on what it can be.

Note for anyone running it: the per-hit lines are capped at 64
(`STEP3_LOG_CAP`), and both audit summary lines print only at normal exit, so a
crashing arm produces no verdict.

### Ruled out 2026-09-03: precise oop maps for >64 locals do NOT fix it

`fix/jit-precise-oop-maps-wide-locals-20260903` ("methods above 64 locals had no
precise oop maps at all") is the closest thing to an unnamed root in a compiled
frame that has landed, and it is NOT this defect. Rebuilt on dev with that fix
in (`2632fb2c1` confirmed an ancestor), the second trigger still SIGSEGVs
**2 of 3** (103 s, 119 s), same page-aligned fault `rdi`.

So the surviving candidates are unchanged: this page's own prediction of
scalar-replacement, LICM-hoist or GPR-spill slots -- none of which the runtime
oracle corroborates either (`verifier_oop=0`). Whatever names the stale
reference, it is not a Java local above the 64 mark and not something the class
file's type maps call a reference.

### 2026-09-03: the unnamed root IDENTIFIED, and three fixes that do not work

A detector that asks the failing question directly --
`CRATONVM_DBG_STALE_FRAME_WORDS=1`, which audits each frame AFTER
`remap_one_jit_frame` has rewritten every slot the maps name and reports any
word still holding an address this collection moved. On
`TestCachedQueryResults`: `frames_audited=375 stale=390`, and the shape is the
finding:

```
queryCounter [rbp-0x178] gpr-safepoint-spill stale=0x1fef6b70878 should_be=0x1fee1865758
queryCounter [rbp-0x58]  operand-spill       stale=0x1fef6b70878 should_be=0x1fee1865758
queryCounter [rbp-0x28]  java-local          stale=0x1fef6b70878 should_be=0x1fee1865758
queryCounter [rbp-0x18]  java-local          stale=0x1fef6b70878 should_be=0x1fee1865758
```

ONE object, FOUR slots, four storage classes, `maps=7 covered=true`. The
register allocator keeps several copies of a reference and the map names the
canonical home. Confirmed at scale by a second pass:
`duplicate_of_mapped=47946355`.

That also explains why `CRATONVM_DBG_VERIFY_OOP_MAPS` never found it: that
oracle asks whether the CLASS FILE calls a slot a reference and answers
`verifier_oop=0` over 2.8 M candidates. These are compiler-introduced copies the
class file's model does not mention, so it is structurally blind to them.

**Eliminated, each by measurement:**

| candidate | how it was ruled out |
|---|---|
| map SELECTION | `NO_MAP_FOR_SP_ID=0`, `NO_SP_ID_SLOT=0` |
| precise oop maps for >64 locals | that fix in, 2/3 still SIGSEGV |
| the box/unbox intrinsic | disabled in every run here |
| the register image | `CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1` reports ZERO |
| pins not reaching the high end | `compact_high_region` takes the same `pins` and marks overlaps `immovable` |

**Three fixes tried, none works, each failing informatively:**

1. `CRATONVM_JIT_PIN_UNNAMED_FRAME_REFS=1` -- pin every object a frame names in
   an unmapped slot. `frames=10357698 pinned=96587490`, and 2 of 3 still
   SIGSEGV. Pins ARE honoured by both relocation paths, so the object that kills
   it was never in the pin set.
2. `CRATONVM_JIT_REMAP_UNMAPPED_DUPES=1` -- rewrite them instead. Only
   `frames=285 rewritten=68`: the remap runs on relocating cycles over frame
   BANDS, three orders of magnitude less reach than the scan-time pass, and 3 of
   4 still SIGSEGV.
3. Both together with the shadow-stack scan -- unchanged.

**What that leaves.** The duplicates are real, enormous and NOT sufficient to
explain the crash. The killing reference is in none of: a named map slot, an
unnamed frame slot, the register image, the shadow stack, or the conservative
stack scan. `relocate_stw`'s own comment names the remaining possibility -- "a
pointer that never left a register is not in it" -- and this workload's failing
allocation is a 524304-byte reference array, i.e. the large-object end.

The next step is NOT another slot-scanning variant. It is either making the oop
maps name every home the register allocator creates (codegen), or having ZGC
consult the guard `gen_heap` and `g1` both consult and this collector, by its
own comment here, "read ZERO times".

### The blanket guard removes this crash -- at a price that rules it out

`CRATONVM_ZGC_JIT_BLANKET_REFUSAL=1` (new) applies `gen_heap`'s and `g1`'s rule
here: a live compiled frame refuses relocation, proof or no proof. On the
`TestCachedQueryResults` trigger it is **0 SIGSEGV in 4 runs**, against a
same-binary control that crashed.

That is a useful confirmation -- it means this defect really is confined to
relocation under live compiled frames, with no residue elsewhere -- and it is
not a fix anyone can ship: the same 4 runs log 8-9 k fragmentation
`OutOfMemoryError`s and never complete, where the unguarded arm finishes in
462 s with ZERO. Full numbers on the H2 page.

### 2026-09-04: FIVE remedies measured; the elimination inference was WRONG

| remedy | what it covers | SIGSEGV | OOM |
|---|---|---|---|
| pin unnamed frame refs | this thread's frame slots (96 M pins) | no fix | -- |
| rewrite unmapped dupes | this thread's frame slots (68 words) | no fix | -- |
| `local_mask_unreached` fail-closed | this thread's safepoint maps | 3 / 4 | **0** |
| **blocked-wake JIT remap** | **blocked PEERS' frames + regs + shadow** | **3 / 4** | **0** |
| blanket guard | any thread in JIT | **0 / 4** | ~9700 |

The entry below inferred, from "only the peer-covering remedy works", that the
defect was a blocked peer resuming with un-remapped compiled state. That
inference is REFUTED: `apply_pending_blocked_fixups` now remaps a waking
thread's JIT frames, register image and shadow stack -- exactly that population
-- and the crash is unchanged at 3 of 4, against a control at 2 of 2.

The omission was real and is worth keeping (see below); it was not this crash.

**What that leaves.** The blanket guard refuses when `is_active()` -- ANY thread
in compiled code, including the INITIATOR and cooperatively PARKED peers, not
just blocked ones. Four remedies have now covered: this thread's frame slots,
this thread's safepoint maps, and blocked peers' full compiled state. The
population none of them reaches is an OS-SUSPENDED in-JIT peer -- the `taken`
threads of the xt scan, stopped mid-compiled-code by signal or `SuspendThread`,
whose machine registers are captured by the scanner but which run no
`apply_pointer_map_to_thread` of their own. That is the next place to look, and
it is the last population the guard covers that nothing else does.

**Two omissions closed on the way, both real, both free, neither this crash:**

* `local_mask_unreached` -- a safepoint whose local-oop dataflow was never
  reached shipped a map claiming complete coverage while naming none of its live
  reference locals, `125` and dominant on this workload, while the SHADOW half
  counted the same population and refused. Zero measured OOM cost.
* the blocked-wake JIT remap -- `apply_pending_blocked_fixups` remapped
  interpreter frames and nothing compiled, so a peer that blocked with compiled
  frames below it resumed with every JIT oop at its pre-move address. The
  STW-resume path has carried this block for the PARKED case since it was found
  there. Zero measured OOM cost.

Both are use-after-free shaped, both cost nothing, and both should land on their
own merits rather than waiting on the crash they do not fix.

### SUPERSEDED (2026-09-03): four fixes, and the one that works says WHERE the defect is

| attempt | what it covers | SIGSEGV | OOM cost |
|---|---|---|---|
| pin unnamed frame refs | this thread's frame slots (96 M pins) | no fix | -- |
| rewrite unmapped dupes | this thread's frame slots (68 words) | no fix | -- |
| `local_mask_unreached` fail-closed | this thread's safepoint maps | 3 / 4 | **0** |
| **blanket guard** | **ANY thread in JIT, peers included** | **0 / 4** | ~9700 |

Every targeted repair addresses the CURRENT thread's compiled frames, and none
works. The only thing that works is the one that also covers PEERS. That is the
diagnosis, by elimination with a positive control in every batch.

And the mechanism is sitting in the blocked-wake path:
`vm_exec::apply_pending_blocked_fixups` contains **ZERO** calls to
`remap_active_jit_frames`, `remap_register_image_words` or
`shadow_stack.remap`. It remaps a blocked thread's INTERPRETER frames,
`printed`, `java_thread_obj` and `native_pin_roots` -- and nothing else. So a
peer that blocked with compiled frames below it resumes with every JIT-frame oop
at its pre-move address.

Pinning those peers is what the ZGC pinned-peer credit does
(`bug-h2-testcachedqueryresults-zgc-oom-livelock-20260829.md`), and pinning is
not sufficient: a pin withholds the PAGE, and the conservative scan that finds
what to pin cannot see a reference that never left a register. Hence the guard
-- which refuses whenever any thread is in JIT -- being the only effective
remedy, and an unaffordable one at ~9700 fragmentation OOMs.

**The fix is rewritability, not immobility**: `apply_pending_blocked_fixups`
must remap the waking thread's JIT frames, register image and shadow stack, the
way `apply_pointer_map_to_thread` does on the STW-resume path. That is a change
to the blocked-wake path and it is the remaining work.

Two ruled-out-by-checking notes for whoever takes it:

* the `gpr-safepoint-spill` region is WRITE-ONLY (`emit_blind_reg_spill` stores;
  `emit_post_safepoint_reload` reloads from CANONICAL slots), so stale words
  there are harmless -- do not "fix" them;
* `local_mask_unreached` is a REAL hole (125 on this workload, dominant, while
  the shadow half counted the same population and refused) and worth fixing on
  its own merits -- it is simply not this crash. PRICED: 4 runs with
  `CRATONVM_JIT_LOCAL_MASK_UNREACHED_FAIL_CLOSED=1`, **ZERO OOM on every arm**,
  the completed run still relocating freely (`compaction_cycles=24`,
  `relocation_on_proven_jit=24`, `relocation_skipped_jit=3`) and producing
  99959. Regression suite 88/88. The refusal fires on ~125 safepoints, a thin
  slice, so it costs nothing measurable -- unlike the blanket guard's ~9700
  OOMs and total loss of completion. Recommended to land default-ON on that
  evidence; it is a correctness hole with no measured price.

  That zero is also the discriminator: had the OOMs climbed toward 9700, a clean
  crash result would have been the guard in disguise. They did not move at all,
  which is what confirms whatever suppresses the crash in the guard arm is the
  PEER coverage and not per-safepoint map completeness.

## The mitigation — SUPERSEDED, the default is back ON

For two days `box_unbox_intrinsic_disabled()` defaulted to disabled and
`CRATONVM_JIT_BOX_UNBOX_INTRINSIC=1` turned the family back on. **That is no
longer the shipped state.** `069e67b43` restored the default to ON once the
crash was root-caused to the collector rather than to this intrinsic: the
intrinsic's only part was raising allocation pressure enough to make the slide
run, which is why turning it off hid the crash and why turning it off was never
a fix.

`CRATONVM_JIT_NO_BOX_UNBOX_INTRINSIC=1` still forces it off and still means the
same thing, so any script that already sets it is unaffected.

The speedup this section promised was "recoverable the moment the sequence is
made relocation-safe". The sequence never was the problem; it was recovered by
fixing the collector.

## The repro was BLOCKED by an earlier failure (2026-09-02) — CLEARED

**That blocker is fixed.** `seed:0 op:1033 AssertionError: (1810, null)` was the
guarded-inline native screen asking the callee's DECLARING class when the native
is registered on the concrete receiver class, so a compiled
`for (e : treeMap.tailMap(k).entrySet())` iterated zero entries:
`internal/fixed-bugs/guarded-inline-native-screen-asked-the-declaring-class-FIXED-20260904.md`.
The section below is kept for its method note — which is still the right advice
for scoring a bisect — and its "whoever takes this page next has to clear that
first" instruction no longer applies.


Run on `dev@08a1711e5`, `livedbg`, quiet host, against H2 built at
`apps/h2database/h2`. **It cannot reach the window this page measures in.**

`TestRandomMapOps` dies of `seed:0 op:1033 java.lang.AssertionError: (1810,
null)` after 11-22 s, having completed ZERO passes -- where HotSpot 25 on the
same classpath completes at least nine. The SIGSEGV this page records appears at
25-183 s. The run is over before that window opens.

It is not this intrinsic. Five runs per arm at `--Xmx 256m`, three per arm at
384m / 512m / 1g:

| arm | outcome |
|---|---|
| `CRATONVM_JIT=box-unbox-intrinsic` (family ON) | AssertionError, 0 SIGSEGV |
| shipped default (family OFF) | AssertionError, 0 SIGSEGV |
| `CRATONVM_ZGC_RELOCATE=0` | AssertionError, unchanged |
| `CRATONVM_JIT_NO_INLINE_FRAME_MAP=1` | AssertionError, unchanged |

Same op, same values, every arm. And it is **deterministic**: `seed:0 op:1033
(1810, null)` byte-identical across four consecutive runs.

That contradicts two things this page and its sibling rest on. This page says
the failures pre-dating the bisect range are a `NullPointerException` and a
fragmentation `OutOfMemoryError`; the blocker is neither, so a bisect scored the
way this page describes would now score every commit BAD for the wrong reason.
And `h2/bug-h2-testrandommapops-small-heap-corruption-20260829.md` says `--Xmx
1g` and `4g` are "clean over 1500 s each" -- at 1g this fails in 13-15 s. That
page also calls its defect one with "no reproducer worth bisecting yet". It has
one now, and it is 13 seconds long.

**Whoever takes this page next has to clear that first**, or bisect the SIGSEGV
on a tree where `op:1033` does not fire.

Not established, and measured to be unavailable rather than assumed away:
**whether the blocker is JIT-dependent.** A `--nojit` arm ran the full 1500 s
with no AssertionError -- and completed ZERO passes, where HotSpot completes one
about every 15 s. `TestRandomMapOps` prints an `op:` line only when it FAILS, so
a run that has not failed offers no evidence it ever reached op 1033. "1500 s
clean under `--nojit`" is therefore not a result; it is a run that may simply
be slower than the defect is deep. Scoring it as an arm would be the same
mistake as the `objects_relocated=0` probe above.

Making that arm answerable needs a progress signal the test does not currently
emit -- a per-op counter, or a seeded run bounded to a few thousand ops.

## Reproducing

```
cargo build --profile livedbg -p cratonvm-cli

# The classpath file omits H2's own output dirs; both are needed.
H=apps/h2database/h2
CP="$H/target/test-classes:$H/target/classes:$(cat $H/craton-testcp.txt)"

# Run from a scratch cwd: the test writes its database files beside you.
CRATONVM_JIT=box-unbox-intrinsic cratonvm --java-home $JDK25 --Xmx 256m \
  -cp "$CP" org.h2.test.store.TestRandomMapOps
```

Under three minutes on a quiet host. A CONTENDED host hides it -- the same
lever this repo has been bitten by before.

`CRATONVM_JIT_BOX_UNBOX_INTRINSIC=1` still works but now warns; the supported
spelling is the token above.

**As of 2026-09-02 this does not reach the SIGSEGV** -- see "The repro is
currently BLOCKED by an earlier failure".

## What is still open

**One item: `CRATONVM_JIT_LOCAL_MASK_UNREACHED_FAIL_CLOSED` should default ON,
and the evidence for flipping it is stale.**

This page found and priced a real correctness hole that is not this crash: a
safepoint whose local-oop dataflow was never REACHED ships a map claiming
complete frame-slot coverage while naming none of its live reference locals
(`map_incomplete_cause::LOCAL_MASK_UNREACHED`, 125 on `TestCachedQueryResults`
and dominant, while the SHADOW half counted the same population and refused).
Its sibling — `CRATONVM_JIT_LOCAL_MASK_FAIL_CLOSED`, the same hole for a method
the dataflow never ran on at all — already **defaults ON** with a `=0` opt-out.
This one is still opt-in, and `licm.rs` says why: *"this population is larger
and its refusal cost is still being priced"*.

The page's own pricing said ZERO OOM on 4 runs and 88/88, and recommended
default-ON. **That pricing is no longer usable, and not because it was wrong.**
It was taken on 2026-09-03, before the root-cause fix at the top of this page
moved the OOM baseline on this very workload from 1497 to 0. A refusal's cost
has to be measured against the collector that ships, and the collector changed
underneath it. The whole reason to price this at all is that the blanket guard
— the other refusal measured on this family — bought its safety at ~9700
fragmentation OOMs and total loss of completion.

**Attempted 2026-09-05 and not obtained.** An ABBA-interleaved re-pricing on
`org.h2.test.jdbc.TestCachedQueryResults --Xmx 256m` was started on host `vm1`
at load average 29-33. The first arm **timed out at the 1300 s cap** on a
workload this page records completing in 462-728 s quiet, so every arm would
have timed out and a timed-out arm yields no OOM or completion figure to
compare. The run was stopped rather than reported. This page's own repro note
already says it: *"A CONTENDED host hides it."*

To finish it, on a quiet host (load < 5):

```
CVM=<release cratonvm> JDK=$JDK25
H=apps/h2database/h2
CP="$H/target/test-classes:$H/target/classes:$(cat $H/craton-testcp.txt)"
# ABBA, 4 runs per arm, from a scratch cwd; count OutOfMemoryError and `actual`
for arm in A B B A; do
  case $arm in A) F= ;; B) F=CRATONVM_JIT_LOCAL_MASK_UNREACHED_FAIL_CLOSED=1 ;; esac
  env $F CRATONVM_GC_STATS=1 timeout 1300 "$CVM" --java-home "$JDK" --Xmx 256m \
      -cp "$CP" org.h2.test.jdbc.TestCachedQueryResults
done
```

Flip it when the fail-closed arm shows no OOM increase and still completes —
inverting `local_mask_unreached_fail_closed_enabled` in `jit/src/x64/licm.rs`
to the `=0`-opt-out shape its sibling already uses, and changing the
`flag_groups.rs` row from `on_key` to `off_key`. **Do not flip it on the
2026-09-03 numbers.**
