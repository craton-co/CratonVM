# Proposal: one per-safepoint live set for optimizing-tier frames, spent by every channel

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 10
> of 54).** Not built. The default-on precision since this page was written
> (`CRATONVM_JIT_IR_DEAD_HOME_CLEARS`, `CRATONVM_JIT_IR_PRECISE_KEEP_SET`,
> `CRATONVM_JIT_IR_KEEP_SET_DEF_KILLS` with d5/u's phi kills,
> `CRATONVM_JIT_IR_DEAD_HOME_VALUE_RANGES` since d6/u) adds two more switches
> to the four it would retire, and fixed the OsrDeadSlot probe (d7
> osr_dead_1..3 `PASS all 3`; controls osr_dead_vr0_1..2 fail). The long-term
> design is unchanged. **Gate:** the page's first step: the exact live set as
> a second map field, counted at every collection against the oracle; zero
> contradictions over the battery and the netty / Spring Boot suites is the
> entry ticket for items 2-4. **Size:** L.

*Filed 2026-09-27 by gen round 5 wave 5, lane `oomjit9`. A direction, not a
defect: the defects it would retire are on
`gengc-r5w5-oomjit9-ir-dead-homes-residuals-20260927.md`.*

## Where the tier is now

Five waves have each added one more way of saying "this word of an IR frame
is dead": the opt-in store form (`CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`), the
opt-in store-free claim (`CRATONVM_JIT_PRECISE_FRAME_LIVENESS`), the opt-in
whole-frame prologue zeroing, and now the default-on dead-home clears
(`CRATONVM_JIT_IR_DEAD_HOME_CLEARS`). All four derive from the SAME
`DeadRefSlotClear` facts, and each is spent by a different subset of the
three channels that root a frame (frame block, band scan, safepoint map).
The frame's maps still name every `Ref` home emitted so far in linear order,
and its `live_frame_hi` is a watermark. HotSpot's C2 publishes, per
safepoint, exactly the oops live there, and the collector reads nothing else.

## Proposal

1. **Emitter.** At every safepoint the lowerer already knows (a) which
   colours are live by range, (b) which are named by a snapshot a deopt at or
   after this point can consult (bytecode reachability + the schedule's
   `after_kept` + splices, gen r5w5), (c) the parameter homes. Publish that
   set -- not `defined_nodes` -- as the map's `frame_slot_offsets`, with
   `stack_marks_exact`, and narrow (b) by bytecode local liveness (residual
   item 3).
2. **Frame block.** Keep publishing once per activation, but make the
   collector resolve each indirect entry through the active map: an entry
   whose slot the active safepoint's map does not name is skipped. That is a
   one-lookup filter in `ShadowStack::for_each_value_in` given the frame's
   `(rbp, cm)`, which the band walk already has; the entries can be grouped
   per frame (the block is contiguous and its saved-base word marks it).
3. **Band scan.** For a frame whose body publishes exact maps, root only the
   map's words plus the non-colour regions that are not claimed (callee-saved
   images, deopt register image) -- i.e. what `scan_one_frame_filtered`
   already does for the single-pass tier with its cursor and mask.
4. **Delete** the store forms once the map is exact: nothing is written into
   compiled code, so the tier pays zero cycles for precision at run time, and
   the four switches collapse into one `=0` bisect lever.

## Why it is worth the work

- The default clears cost a store per value that died since its last call,
  at every call; an exact map costs nothing at run time.
- The allocation-site hole (residual item 1) disappears without arming a
  single allocation.
- The collector gets a relocatable, precise root set for the whole tier,
  which is what the moving-young coverage proof has been approximating with
  `moving_young_coverage_complete`.

## Cost and risk

The collector side (items 2-3) is lane pin9's code
(`vm/src/jit/conservative_roots.rs`, `gc/src/shadow_stack.rs`). A wrong exact
map frees a live object, so the oracle
(`CRATONVM_DBG_VERIFY_REG_OOP_MAPS=1`, the `[liveset]` contradiction counter)
must run over the regression battery and the netty / Spring Boot suites before
any default. Keep the conservative fallback for every frame whose body does not
publish exact maps.

## First step

Emit the exact live set as a SECOND field beside `frame_slot_offsets` (no
consumer), count at every GC how many band words it would exclude and how many
of those the oracle finds reachable. Zero contradictions over the battery is
the entry ticket for items 2-4.
