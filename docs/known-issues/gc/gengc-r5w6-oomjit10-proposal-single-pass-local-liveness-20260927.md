# Proposal: single-pass frames stop rooting their dead locals

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 2
> of 54).** Not built. The probe it cites fails on d7 even on the DEFAULT arm,
> not only under `CRATONVM_JIT_OSR_OPTIMIZING=0`: w34_ngr_1..3 and
> w34_ngr_compat_1..3 (`-XX:+UseGenerationalGC -Xmx128m
> NativeGrowthReclaimProbe`, jdk-only and `--compatible`) exit rc=1 with an
> `OutOfMemoryError` escaping `NativeGrowthReclaimProbe.fillAndDrop`, 6/6; the
> holder is not attributed (no census row), so this page is the first suspect,
> not a proven cause. The optimizing tier's half of the idea is default on
> (`CRATONVM_JIT_IR_PRECISE_KEEP_SET`,
> `CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES` since gcd d6/u). **Gate:** first a
> holder census on w34_ngr (`CRATONVM_DBG=oldmark-root-census,root-source`): a
> `region=java-local` holder in a single-pass frame makes this the fix; then
> the page's own gate (HotSpot's lines on the four probes under
> `CRATONVM_JIT_OSR_OPTIMIZING=0`, `[liveset] contradictions=0`, deopt tests
> unchanged). **Size:** M.

*Filed 2026-09-27 by gen round 5 wave 6, lane `oomjit10`. A direction, not a
work item of the wave: it changes what a moving young collection rewrites, so
it wants a measured gate before it is default.*

## The gap

Gen r5w6 made the OPTIMIZING tier's frame roots liveness-exact at call sites
(`CRATONVM_JIT_IR_PRECISE_KEEP_SET`: snapshot locals narrowed by bytecode
liveness, a keep set with kills, clearable phi homes). The SINGLE-PASS tier
still roots every local its TYPE dataflow calls a reference:

- the precise map (`x64/safepoint.rs::emit_oop_map_for_safepoint`, Stage 2)
  names local `k`'s canonical slot whenever `local_oop_masks[pc]` has bit `k`
  -- a type fact ("holds a reference here"), not a liveness one;
- the band scan (`conservative_roots::scan_one_frame_filtered`) reads the whole
  `java-local` band conservatively; none of its five claims speaks for it (the
  liveness table, `is_dead_by_frame_liveness`, is only ever published by the
  optimizing tier).

So a single-pass `main` that is done with a 40 %-of-heap array keeps it until
the slot is reassigned -- `NativeGrowthReclaimProbe` under
`CRATONVM_JIT_OSR_OPTIMIZING=0`, and any method the optimizing tier refuses.
HotSpot's C1 and C2 maps drop dead locals.

## Why it is sound to drop them

Every reader of a single-pass local slot at a safepoint either wants the
local's value while it is LIVE, or tolerates any value:

1. **Deopt / exceptional frames** describe a dead local as
   `FrameValue::Undefined` already (`x64/deopt_stubs.rs::local_liveness_word`,
   over `regalloc::live_locals_per_pc_all` with handler edges), except the
   `local_assigned` set a debugger-observed compile keeps describable from its
   home -- and under a debugger the rule must be off anyway, as the
   optimizing tier's clears are (`CompileRequest::debugger_observes_locals`).
2. **The post-safepoint reload** (`emit_post_safepoint_reload`) reloads every
   oop local's register from its slot. For a dead local the register is never
   read before it is written, so a stale value there is harmless -- but see the
   moving-young point below.
3. **The collector** is the reader this proposal narrows.

## What to build

1. At every GC-capable safepoint the single-pass compiler records, per
   safepoint id, the java-local slots it WROTE for a reference local
   (`covered`) and those of them bytecode-live at `cur_bc_pc` (`live`) --
   exactly a `crate::FrameLivenessTable` record, the structure the optimizing
   tier publishes under `CRATONVM_JIT_PRECISE_FRAME_LIVENESS`. The liveness is
   already computed for the deopt snapshot (`local_liveness`), so this is a
   bitset per safepoint.
2. Leave dead locals out of `frame_slot_offsets` (Stage 2) under the same
   switch, so the precise channel stops rooting them too; the scan side needs
   no change (`is_dead_by_frame_liveness` spends any table a body carries, and
   `note_liveset_contradiction` checks the two halves agree).
3. Moving young: a dead local's slot is no longer REWRITTEN either, and the
   reload then loads a stale from-space address into the dead local's register.
   That register is dead, but the NEXT safepoint's blind spill copies it into
   the image and its mask names it (the mask is type-based, via
   `for_each_oop_local_at_current_pc`). Two options: narrow the mask by the
   same liveness (preferred -- it also drops the image word), or skip the
   reload of dead locals. Either must land with (2).

## Gate before default

- `GenR5W2OsrDeadSlotProbe`, `GenR5W3OsrHolderProbe`, `GenR4W6JitOomRootProbe`
  and `NativeGrowthReclaimProbe` under `CRATONVM_JIT_OSR_OPTIMIZING=0` print
  HotSpot's lines;
- the oracle `CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1` reports `[liveset]
  contradictions=0` and no `unreachable` dropped word on the Spring Boot
  sample and netty `io.netty.buffer`;
- `CRATONVM_JIT_THRESHOLD=1` deopt-heavy tests unchanged (a difference is a
  local the liveness called dead that a resume read).
