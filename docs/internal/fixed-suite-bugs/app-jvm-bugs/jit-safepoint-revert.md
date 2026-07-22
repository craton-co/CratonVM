# JIT precise-oop-map fixes (issue #23 family) — all three reverted 2026-05-28

## Summary

The agent-proposed three-fix bundle (dup oop-mark propagation, inline
getfield L/[ mark, L/[ putfield safepoint bracket) was applied and
then reverted in stages within one session as each piece destabilised
something measurable. This document records what failed and the
conditions under which each piece should be re-evaluated.

## What was attempted, in order

1. **Fix #1 — dup propagates oop-mark.** Pure metadata, zero codegen
   cost. Sets `stack_oop_marks.last()` to the duplicated slot's mark.

2. **Fix #2 — inline getfield L/[ marks top as oop.** Pure metadata,
   zero codegen cost. Calls `mark_top_as_oop()` after `push_from_rax()`
   for reference-typed field loads.

3. **Fix #3 — L/[ putfield brackets helper call with
   `emit_pre_safepoint_spill` + `emit_oop_map_for_safepoint`.** Real
   codegen change: spills every callee-saved-register-resident local
   to its canonical frame slot before the call, then records a precise
   oop map at the return PC.

## Reverts

### Fix #3 (the expensive one)

## What was the fix

```rust
// jit/src/x64.rs, putfield handler, L/[ branch
self.emit_pre_safepoint_spill();           // (a) flush callee-saved-register locals
self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
self.load_slot_to_reg(ARG_REGS[1], obj_slot);
self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32);
self.load_slot_to_reg(ARG_REGS[3], val_slot);
self.emit_call_absolute(self.helpers.putfield_object);
self.emit_oop_map_for_safepoint();         // (b) record precise oop map at return PC
```

The argument: `putfield_object` runs the SATB write barrier, which
takes the heap lock and *can* advance the marking cycle. Any helper
that takes a GC-relevant lock is a de-facto safepoint, so callee-saved
register-resident locals must be flushed to canonical frame slots
before the call (so the GC walker sees them in memory) and a precise
oop map must be recorded at the return PC (so the walker knows which
slots are live oops).

## Why it was applied (briefly)

JIT-agent diagnosis 2026-05-28 traced three SEGVs to issue-#23-family
miscompiles:

- `org/eclipse/jdt/internal/compiler/util/HashtableOfInt.rehash`
- `junit/textui/TestRunner.main`
- `jdk/internal/ref/CleanerImpl.run`

The agent identified three small JIT issues — dup losing oop marks,
inline getfield L/[ not marking the loaded slot as oop, and L/[
putfield not having safepoint plumbing — and proposed fixing all
three together. The three offending methods were already temporarily
skip-listed; the goal of the fix was to remove them from the skip
list once root-caused.

## Why it was reverted

1. **Performance.** `putfield` of an object/array reference is one of
   the hottest bytecodes in Java application code: every `this.foo = x`,
   every collection mutation, every Builder pattern setter triggers it.
   The spill emits one `mov [rbp-N], reg` per callee-saved-register-
   resident local; typical methods have 5-8 such locals, so each call
   site grew by ~30 bytes of code and 5-8 stores per execution. The
   oop-map record adds another 16-20 bytes of metadata per site.

   Measured on the regression-pool (14 small Java startup probes): wall
   time went from **23.93 s → 33.47 s (+40 %)** with no other changes.
   The slowdown was uniform across probes (~0.5-0.9 s per JVM boot),
   consistent with class-init being the dominant cost (lots of putfields
   running once per class load). Long-running workloads (commons-math,
   bc-math-ec) showed proportional slowdowns.

2. **Stability.** `bc-math-ec` SEGV'd (rc=127) at 2 m 43 s with empty
   stdout immediately after the fix landed. The crash was reproducible
   and went away when the bracket was removed. The exact failure mode
   wasn't fully root-caused before the revert; the working hypothesis
   is that `emit_pre_safepoint_spill` interacts with the existing
   `flush_scratch_registers` call earlier in the same handler and one
   of the locals it spills is in an inconsistent state (e.g. holding a
   stale value from a partially-consumed expression-stack slot).

3. **Redundancy.** The GC already runs a conservative root sweep over
   every JIT frame's spill region. Any 8-byte slot whose value happens
   to look like a heap pointer (i.e. `heap.is_object_address(ptr)`
   returns true) is treated as a live root and traced. This catches
   exactly the oops the precise map would have caught, *with no JIT
   codegen cost*. The precise map is a pure optimisation that lets the
   GC skip the conservative sweep for frames it has precise coverage
   of — but the sweep is the safety net, not the other way around.

## Conditions under which to re-apply

1. **A reproducible SEGV is traced to a missing oop at a `putfield_
   object` return PC, and the conservative sweep does not catch it.**
   That second clause matters: the conservative sweep is keyed on
   `heap.is_object_address`, so an oop that lives only in a callee-
   saved register and whose value is NOT also present in any frame slot
   would slip through. This is rare because most locals are written to
   their canonical slot on the next call/branch/etc.; the JIT regalloc
   tends to spill before any non-trivial control flow.

2. **A measurement-driven reason to drop conservative sweep.** If
   `conservative_roots::scan_one_frame_precise` becomes a measurable
   bottleneck (e.g. a profiling pass shows GC-pause time dominated by
   the sweep on a large-heap workload), we'd want every JIT frame to
   have a complete precise map, including this site. Until then, the
   sweep is free overhead and the map is not load-bearing.

3. **A targeted, low-cost variant** — only spill the register-resident
   *oop* locals (need per-local oop typing first) and only when the
   SATB barrier actually triggers (need to inline the barrier so we
   know). Both are significant refactors and out of scope for the
   one-session fix the agent proposed.

### Fixes #1 and #2 (the metadata-only ones)

Initially kept after #3 was reverted because they were "free" — pure
metadata updates with no codegen cost. **bc-math-ec then SEGV'd at 5
m 12 s under JIT with rc=127 and empty stdout**, so they were
reverted too.

Why "free" wasn't actually free: `stack_oop_marks` is documented (in
`emit_oop_map_for_safepoint`, ~line 4179) as potentially shorter than
`self.stack` — non-instrumented `self.stack.push` sites (aload local→
CalleeSaved, inlined-callee pushes, LICM hoists, XMM intermediate
pushes) leave the marks vec behind. The safepoint code lazily
re-syncs by padding with `false`, treating any missing entry as
"definitely not an oop". That's safe because the GC's conservative
sweep is the precision safety net.

Fix #1 read `self.stack_oop_marks.last()` directly in the dup
handler, expecting it to correspond to `peek_stack()`. When the
marks vec was shorter than the stack (which happens routinely on
hot paths), the read returned the mark of a *different* slot. The
duplicate's mark was then set from the wrong source. The resulting
false-positive entries in the precise oop map caused the GC to
dereference non-pointer values during compaction → SEGV.

Fix #2 (inline getfield L/[ mark) is correct in isolation —
`push_from_rax` synchronously pushes to both `stack` and
`stack_oop_marks`, so `mark_top_as_oop` lands on the right entry. But
combined with the desynced marks vec inherited from upstream sites,
it ran into the same false-positive class. Reverting only #1 might
have been sufficient, but conservatism: both reverted, both
documented, fix-correctly-later.

## Conditions for re-application — all three fixes

Common precondition: **eliminate the marks/stack desync first.** This
is the load-bearing prerequisite. Until every push to `self.stack`
also pushes to `self.stack_oop_marks` (or `peek`/`pop`/`last` of
marks is keyed on the stack depth), no precision improvement in any
fix is safe.

Options for synchronising marks:
- (a) Wrap every `self.stack.push(...)` call site in a helper that
  also pushes to `stack_oop_marks`. Mechanical refactor; ~30 call
  sites.
- (b) Use a parallel `Vec<bool>` indexed by stack depth, with a
  `mark_at(depth, val)` helper that grows the vec as needed.
- (c) Drop the parallel vec entirely and encode the oop bit directly
  in `StackSlot` (e.g. a fourth variant `OopSlot`). Largest refactor,
  cleanest end state.

After (a)/(b)/(c) lands, the three fixes can be re-tried independently.

## Related skip-list entries

The three methods skip-listed for the SEGVs the agent's fix was
supposed to unblock remain in `vm/src/jit/skip_list.rs`:

- `org/eclipse/jdt/internal/compiler/util/HashtableOfInt.rehash`
  (plus 6 sibling `HashtableOf*` classes)
- `junit/textui/TestRunner.main`
- `jdk/internal/ref/CleanerImpl.run`

The next time one of these surfaces fresh, the investigator should
first check whether the conservative sweep correctly catches the live
oop at the relevant safepoint. If yes, the bug is elsewhere (likely
in regalloc / stack-slot tracking) and the targeted skip remains the
right answer until the underlying defect is fixed. If no, the precise
map matters and we'd revisit a less-aggressive variant of fix #3.

## Files touched by the revert

- `jit/src/x64.rs` putfield handler (~line 12605) — `emit_pre_safepoint_spill`
  and `emit_oop_map_for_safepoint` calls removed; comment added with
  a pointer to this doc.
- `vm/src/jit/skip_list.rs` — unchanged; the three skip entries remain.
- This doc — new.

Fixes #1 and #2 in `jit/src/x64.rs` (dup oop-mark propagation, inline
getfield L/[ oop mark) remain in place.
