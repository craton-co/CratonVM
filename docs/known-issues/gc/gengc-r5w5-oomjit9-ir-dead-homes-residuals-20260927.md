# What the default-on dead-home clears still leave rooted in a compiled frame

> **STATUS (2026-09-29, gce e1/x): KEEP -- item 5(b)'s type-conflict half is covered by the new band claim, whose own probe still fails on Generational** (`gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed-20260928.md`). **Remaining:** item 1 (arm `Op::New` / `NewArray`, needs a probe) and item 5's dead-but-must-oop half (the single-pass local-liveness proposal).

> **STATUS (2026-09-29, gce e1/f): item 5(b) NARROWED (its type-conflict half
> FIXED IN CODE); items 1, 5(a), 5(c) OPEN, unchanged.**
>
> - **5(b), the single-pass java-local band read liveness-blind:** the census
>   of `NativeGrowthReclaimProbe` on the base named exactly this
>   (`main off=96 region=java-local tier=sp in_map=false`). The new band claim
>   `SpLocalClaim` (`vm/src/jit/conservative_roots.rs`, default on,
>   `CRATONVM_GC_SP_LOCAL_MAP_ROOTS=0`) drops every single-pass java-local home
>   no map of the active safepoint names, when every map of that id carries the
>   locals oracle. That covers a local the must-be-an-oop dataflow does not
>   prove a reference -- a verifier-`top` local, a primitive one, one not yet
>   assigned on every path. It does NOT cover a local that IS a reference on
>   every path but dead by liveness (the map names must-oop locals, not live
>   ones); that half is still
>   `gengc-r5w6-oomjit10-proposal-single-pass-local-liveness-20260927.md`.
>   Run and tests: `gcd-d10f-native-growth-reclaim-osr-main-holder-unnamed-20260928.md`.
> - **Item 1** (arm `Op::New` / `Op::NewArray` in `dead_ref_clear_site_for`,
>   `jit/src/ir_lower.rs`): re-read, still the mechanical change named below
>   plus an `after_kept` entry per allocation site in
>   `plan_dead_home_clears`; not taken without a probe that collects at an
>   allocation with a dead home (none of this round's failing lines is one).
> - **5(a), 5(c):** unchanged.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): items 1 and 5 OPEN, unchanged; item 3's d5/u and d6/u fixes are measured.** The liveness-blind cases this page's item 3 recorded are closed on d7: `GenR5W2OsrDeadSlotProbe` passes 6/6 and its `CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES=0` control fails 2/2 (the OsrDeadSlot page retires this wave). Item 4's opt-in claim still shows up as a failing arm: `GenR4W6JitOomRootProbe` with `CRATONVM_JIT_PRECISE_FRAME_LIVENESS=1 CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1` (battery `w3_jitoomroot_live`) passes seven lines and then livelocks in `oome-thread-exit` (see `../../internal/gc/gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles-FIXED-20260928.md`). Remaining: item 1 (arm `Op::New` / `NewArray`) and item 5 (a)(b)(c).

> **STATUS (2026-09-28, gcd d5/u): items 1 and 5 OPEN, unchanged; a THIRD
> liveness-blind case of item 3 found and fixed.** Gen r5w6's keep-set
> liveness killed a colour at its sole occupant's definition, but never a
> phi's ("their homes are written by edge copies"). A guarded splice's result
> (`ref.get()` on a non-exact receiver) IS a phi, so `cleared(ref)`'s loop
> kept the previous round's referent across its `System.gc()`. Now each
> `Ref` phi whose every incoming edge writes its home is killed at its merge
> block's entry (`jit/src/ir_lower.rs`: `phi_entry_kills`, the `phi_kills`
> argument of `snapshot_colours_live_after_each_call`; default on, same switch
> as d4/o's union kill, `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS=0`). Its run is the
> OsrDeadSlot page's d5/u block. d4/o's union kill was re-reviewed
> adversarially (exception edges, loops, OSR entry, splices, monitor frames,
> LICM/GVN motion of the defining load): no unsound kill found; the lane
> report lists the arguments.

> **Earlier status (2026-09-27, gcd d3/o, family consolidation): items 1 and 5 OPEN,
> unchanged in code; this page is now their ONE home; no probe line fails
> because of them.** Items 2 and 3 fixed (gen r5w6), item 4 moot at default.
>
> - **Item 1** (arm `Op::New` / `Op::NewArray`): also item 3 of the oomjit10
>   page, which now points here. Unpriced and needed by no probe.
> - **Item 5** now carries three halves, each formerly also filed elsewhere:
>   (a) reference PARAMETER homes (was item 2 of
>   `../../internal/gc/gengc-r5w2-oomjit6-compiled-frame-residue-residuals-RETIRED-20260928.md`);
>   (b) the single-pass java-local band read liveness-blind (item 2 of the
>   oomjit10 page; fix: `gengc-r5w6-oomjit10-proposal-single-pass-local-liveness-20260927.md`);
>   (c) a FRESH single-pass frame's operand-spill words that this activation
>   never wrote, rooted where no cursor excludes them (item 1's single-pass
>   half of the oomjit6 residue page; opt-in lever
>   `CRATONVM_JIT_SP_ZERO_SPILL_BAND=1`, fix proposal
>   `../../internal/gc/gcd-d2f-proposal-single-pass-written-words-claim-REJECTED-20260928.md`). By
>   reading, (c) is NOT a java-local issue: the single-pass prologue zeroes
>   every non-parameter local (`jit/src/x64/frames.rs`, `emit_prologue`).
> - Candidate holders of the two failing probes of the family: (c) for
>   `GenR5W2OsrDeadSlotProbe` (c) if the census says `tier=sp`, and for the
>   umbrella's `GenR4W4NativeStringOomProbe` only if its native-door fix
>   does not pass (see those pages).
>
> **Run (orchestrator):** none of its own. Retire when (a), (b), (c) and
> item 1 are fixed or judged not worth fixing.

> **Earlier status (2026-09-27, gcd d2/f): items 1 and 5 OPEN, unchanged in code;
> re-read on `a1fa77603`.**
>
> - **Item 1:** `Op::New` / `Op::NewArray` are control-pinned like
>   `Op::Call` (`jit/src/ir_schedule.rs`, `pinned_anchor`). Arming them in
>   `dead_ref_clear_site_for` (and giving them an `after_kept` entry) is
>   therefore the mechanical change the d1/b block describes. It is still
>   unpriced and needed by no probe.
> - **Item 5:** the single-pass operand-spill band now has an opt-in
>   fresh-frame zeroing, `CRATONVM_JIT_SP_ZERO_SPILL_BAND=1`
>   (`jit/src/x64/frames.rs`, `Compiler::emit_prologue`). Parameter homes in
>   both tiers, and the single-pass java-local band, are unchanged.
> - **Found while re-reading (by reading only; not a defect of this page's
>   items):** the IR shared deopt stub (`ir_lower.rs`, `emit_deopt_stub`)
>   retracts `top` only under a frame block. Its comment claimed that
>   `ir_deopt_entry` had already unwound `top`, but `jit/src/deopt.rs`
>   `ir_deopt_entry` does not touch the shadow stack. The comment is
>   corrected; the code is unchanged. A value push outstanding at a guard
>   (only possible between a call's push and its reload) therefore stays
>   rooted until the caller's own reload or the VM boundary heals `top`.
>   That is bounded retention and no corruption, because a moving cycle
>   rewrites pushed values in place. The census label
>   `jit-shadow-stack-value` makes such a holder visible.
>
> **Run (orchestrator):** none of its own. Retire with the single-pass
> local-liveness proposal, as the d1/b block says.

> **Earlier status (2026-09-27, gcd d1/b): items 1 and 5 OPEN, unchanged in code;
> no probe of this round is known to need them.** The probe failures this
> page was the fallback explanation for are explained elsewhere:
> `NativeGrowthReclaimProbe` / `GenR4W4NativeStringOomProbe` by the JIT
> helpers' latched overhead limit (fixed this round, umbrella page), and
> OsrDeadSlot (c) is now measurable by tier (the census `prov=` ends
> ` tier=ir|sp sp= live_hi= in_map=`, `conservative_roots::band_word_context`)
> -- a `tier=sp` answer there is item 5's single-pass half. Item 1 (arming
> `Op::New` / `Op::NewArray`) stays a priced change: no probe shape collects
> at an inline allocation with a dead home, and the stores land in allocation
> loops. Keep the page open for those two items; retire it with the
> single-pass local-liveness proposal.

> **Earlier status (2026-09-27, gen r5w6/oomjit10): items 2 and 3 FIXED, default on;
> item 4 moot; items 1 and 5 OPEN (unchanged).** Landed in
> `jit/src/ir_lower.rs` behind ONE new switch,
> `CRATONVM_JIT_IR_PRECISE_KEEP_SET` (default on, `=0` restores the gen r5w5
> keep set byte for byte):
>
> - **the keep set is a liveness, not a union** (the loop half of item 3):
>   `snapshot_colours_live_after_each_call` / `live_colours_after_calls` run a
>   backward liveness over the schedule in which a node reads the frame
>   states the old union credited it with and KILLS the colour it defines
>   when it is that colour's sole occupant (a snapshot-named value is pinned,
>   so always). A value the loop recomputes -- `cleared(ref)`'s `ref.get()`
>   result, named by the `ifnull` of the NEXT round -- is no longer kept
>   across this round's `System.gc()`. The bytecode-reachability union is kept
>   only for snapshots no scheduled node reads;
> - **snapshot locals are narrowed by bytecode liveness** (item 3):
>   `regalloc::live_locals_per_pc_all` over the method's own code with every
>   exception-table row (`DeadRefClearBytecode`, built by the `lib.rs` IR
>   door), so a local the method is done with (`NativeGrowthReclaimProbe`'s
>   `s` / `cs` after their loop) is named by no later snapshot. Stack and
>   monitors are never narrowed, nor a splice's or a non-instruction bci's
>   snapshot;
> - **phi homes are clearable** (item 2): phi colours join `ref_colours`; a
>   phi no compiled code reads (`SlotPlan::phi_code_unused`) has no range to
>   respect; the stale-set walk records each edge copy as a `Def` at the end
>   of the predecessor, and a loop header's phis start stale at entry (an OSR
>   stub seeds them rather than zeroing them).
>
> Item 4 is moot at default (the clears zero the whole stale staging region).
> Item 1 (allocation GC points) and item 5 (parameter homes; the single-pass
> tier) are unchanged; item 5's single-pass half has a proposal,
> `gengc-r5w6-oomjit10-proposal-single-pass-local-liveness-20260927.md`.
> Unit tests: `ir_lower::dead_ref_slot_clear_tests::{a_recomputed_value_is_dead_at_the_call_before_its_next_definition,
> a_call_keeps_its_own_state_and_a_later_read_in_its_block}`.
>
> **Run (orchestrator):**
> ```
> cargo test -p cratonvm-jit --lib dead_ref_slot_clear_tests
> P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
> timeout 300 cratonvm $P GenR5W2OsrDeadSlotProbe
> CRATONVM_JIT_IR_PRECISE_KEEP_SET=0 timeout 300 cratonvm $P GenR5W2OsrDeadSlotProbe   # wave-5 answer
> javac -d out tools/probes/NativeGrowthReclaimProbe.java
> cratonvm -XX:+UseGenerationalGC -Xmx128m -cp out NativeGrowthReclaimProbe
> ```
> Expected at default (= HotSpot Serial): `PASS osr-tail-static`,
> `PASS osr-tail-arg`, `PASS osr-tail-local`, `PASS all 3`; and
> `NativeGrowthReclaimProbe alCapacity=4/4 alAdd=4/4 toCharArray=4/4
> sbCapacity=4/4 PROBE-OK (sink N)`. `CRATONVM_DBG_IR_SLOTS=1` prints the
> per-site `[ir-slots] dead-home clears at nN: offsets [...]`, which now
> includes phi homes. If `NativeGrowthReclaimProbe` still dies at line 95 the
> census names the holder (it now explains the largest root-reached old
> objects when no marker is staged); a `region=java-local` holder at a
> single-pass `main` frame is item 5.

*Filed 2026-09-27 by gen round 5 wave 5, lane `oomjit9`, from the adversarial
review of the optimizing tier's frame roots (`jit/src/ir_lower.rs`) done while
landing `CRATONVM_JIT_IR_DEAD_HOME_CLEARS` (default on). No cargo was run
(lane rule); every claim is by reading.*

- **Severity:** retention under the JIT (data a program dropped stays
  reachable through a live compiled frame; after an `OutOfMemoryError` the
  next allocation can fail again). No corruption: every item below is a word
  that is kept, never one that is dropped.
- **Owner:** `jit/src/ir_lower.rs` (the JIT lane), except item 5.

## Background

An optimizing-tier (IR) frame reaches the collector through three channels
that all over-approximate liveness:

1. the **frame block** -- every planned `Ref` home published ONCE per
   activation as an indirect shadow-stack entry (`emit_frame_block_ensure`);
2. the **band scan** below `live_frame_hi`, which for this tier is the
   monotone spill watermark (`spill_high_water`), not a live cursor, and with
   no register mask (`reg_oop_mask: None`);
3. the **safepoint maps**, whose `frame_slot_offsets` name every `Ref` node
   emitted so far in linear order (`defined_nodes`), live or dead.

Since gen r5w5 the default clears zero, at the first byte of every armed
`Op::Call`, the dead homes that may still hold a value and the stale
argument-staging words; all three channels then read null there. The
following words are outside that rule.

## 1. GC points that are not calls

Only an `Op::Call` is armed (`dead_ref_clear_site_for`). A collection reached
from an inline allocation's slow path (`Op::New` / `Op::NewArray` refilling a
TLAB, or throwing an `OutOfMemoryError` from compiled code) sees the frame's
homes as they were after its last call. A value that died between that call
and the allocation stays a root for the collection the allocation triggers.

*Fix:* arm allocation nodes too. The keep rule already has the schedule's
answer for "what runs after this node" (`DeadRefSlotClear::after_kept`), so
the bytecode-reachability half does not need the node to be control-pinned.
Price it: it puts stores in front of allocations in allocation loops.

## 2. Phi homes are never cleared

`plan_dead_ref_slot_clear` leaves every colour that holds a phi out of
`ref_colours`. A loop-carried reference (a local reassigned every round of an
outer loop) therefore keeps the PREVIOUS round's value in its phi home until
the next back edge copies over it -- for the whole of the next round. In
`NativeGrowthReclaimProbe` that is one round's 26 %-of-heap `ArrayList` beside
the current round's, if the builder keeps the header phi.

*Fix:* a phi's home is written only by its edge copies; a phi is dead at a
call when no range covers it and no reachable snapshot names it, exactly like
any other value. The comment that excludes them ("their slot is written by
edge copies at positions outside the phi's own range") is an argument about
the copy positions, which the forward stale-set union already models if an
edge copy is treated as a `Def` at the end of the predecessor. Needs a unit
test with a loop.

## 3. Snapshots name dead locals, so the keep set is liveness-blind

The builder records a snapshot at every bytecode boundary naming every local
(`ir_optimize.rs`, "the builder records a snapshot at *every* bytecode
boundary"). A local that bytecode liveness says is dead, but whose slot still
names a value, is therefore KEPT at every call from which that bci is
reachable -- including every call in the loop whose header names it.

*Fix:* narrow `named` by bytecode local liveness (`PollExitBytecode` already
computes it for the mode exits; `regalloc::live_locals_per_pc_all` covers
handler edges). A deopt then materialises null for a dead local, which the
interpreter never reads. Not with a debugger attached (`debugger_observes_locals`
already withholds the code, so nothing is cleared then).

## 4. The store-free claim keeps staged arguments a direct call never staged

`precise_liveness_claim` (opt-in `CRATONVM_JIT_PRECISE_FRAME_LIVENESS`) calls
staging words `0..num_args` "this call's own argument", hence live. A direct
call to a compiled callee stages nothing on its hot path
(`CRATONVM_JIT_IR_COLD_ARG_STAGE`), so those words hold the PREVIOUS call's
arguments for the whole callee. That is `GenR4W6JitOomRootProbe`'s
`catch-inline-receiver` failing under the flag. The default clears zero the
whole stale region at the call's first byte, so with them on this is moot; the
claim alone is still wrong about it.

*Fix:* under the claim, call staging words live only on the paths that stage
(the cold paths re-stage before reading), or simply rely on the default clears.

## 5. Parameter homes, and the single-pass tier

Reference PARAMETER homes (`[rbp - (idx+1)*8]`, `region=java-local`) are not
colours and are never cleared: a parameter the method is done with stays a
root for the activation. Single-pass frames (`CRATONVM_JIT_OSR_OPTIMIZING=0`,
or any method the IR refuses) get none of this; their operand-spill words
above the live cursor are already excluded, but their java-local band is
scanned liveness-blind. Owner of the single-pass half: `jit/src/x64/*`.

## How to verify each item

```
P="--java-home $JDK -XX:+UseGenerationalGC -Xmx64m -cp tools/bench"
CRATONVM_DBG=oldmark-root-census,root-source CRATONVM_DBG_JIT_ROOTSCAN=1 \
  timeout 600 cratonvm $P <probe> 2>census.log
grep -n '^\[holder-census\]' census.log
```

A remaining holder with `prov="method=... region=operand-spill"` whose
`method=` frame is at an allocation (item 1) or in a loop (items 2, 3) is one
of these; `CRATONVM_DBG_IR_SLOTS=1` prints `[ir-slots] dead-home clears at
nN: offsets [...]` per armed call, so an offset missing there names the word.
