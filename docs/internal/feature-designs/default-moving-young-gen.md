# Default Moving / Compacting Young Generation

Status: design / not started. XL, GC-coupled. Goal is to close the
Binary-Trees-18 ~23x throughput gap by making a *moving* young gen the default
without regressing the correctness invariant.

## Goal

Make a moving (compacting / bump-allocated, semispace-copy) young generation
the **default** collector, safely — eliminating the allocation/throughput
penalty of the non-moving free-list sweep that runs whenever JIT frames are
live, while preserving the Binary-Trees-18 correctness invariant
(**checksum = 68332206 = HotSpot**).

## Current state (cited)

- **Default = non-moving generational mark-sweep.** `gc/src/gen_heap.rs`
  module doc (`:7`) describes the young gen as a non-moving free-list
  mark-sweep + card table; old gen is also non-moving (`:349`).
- **The JIT-frame safety gate is the crux.** `collect_garbage_inner`
  (`gen_heap.rs:2194`) refuses to run a *moving* (Cheney) collection while any
  JIT frame is live (`:2207` SAFETY comment): JIT frames hold raw object
  pointers in spill slots / registers that are described **only conservatively**
  (`conservative_roots::scan_active_jit_frames`). A semispace copy would
  relocate those objects but cannot safely rewrite a conservatively-discovered
  slot (a stack word that *looks* like a pointer might be an `i64`). So while
  `gc_quiescence::is_active()` the collector runs `sweep_young_non_moving`
  (`:2267`–`:2274`) instead.
- **Selective promotion is the correctness fix and is DEFAULT-ON.** The
  comment at `gen_heap.rs:2243` ("Fix A (2026-06-05)") is the authoritative
  record: under live JIT frames the correct young collector is the **non-moving
  sweep + selective promotion** (`selective_on` in `sweep_young_non_moving`),
  giving **bt18 = 68332206 = HotSpot**. It marks conservatively (over-marking is
  safe), pins conservative roots, and tenures only heap-interior nodes, so no
  live `make`/`check` node is lost.
- **The moving Cheney UNDER-COUNTS bt18.** Same comment (`:2248`): the moving
  semispace produces the long-mislabelled "golden" **67674804** because a
  semispace cannot pin a conservative JIT root nor rewrite a register-resident
  one — some live nodes go stale after the swap. `CRATONVM_DBG_FORCE_MOVING`
  (`gen_heap.rs:2242`) forces the (under-counting) moving cycle for diagnostics
  only.
- **Shadow-stack precise maps exist but are incomplete and default-off.**
  `gc/src/shadow_stack.rs` (276 lines) publishes the operand-stack `Reg` oops
  the conservative scan misses (register-invisibility). The
  `CRATONVM_SHADOW_STACK` gate (`gen_heap.rs:2266`) currently routes those oops
  to be **scanned as roots → pinned** under the non-moving sweep (keeping bt18 =
  68332206), *not* to enable safe moving. The original moving-via-precise-roots
  attempt is recorded as **incomplete** (68199090, a partial fix toward
  68332206, not the answer). Background: `MEMORY.md` "precise JIT stack maps"
  project entry and `docs/internal/app-jvm-bugs/precise-jit-stack-maps-{design,
  findings,followups}.md`.

Net: the *correctness* problem (bt18 checksum) is already solved by the
non-moving sweep + selective promotion. The *performance* problem — non-moving
free-list allocation is much slower than a bump pointer, and the sweep's O(n)
walk dominates allocation-heavy workloads like bt18 — is what a default moving
young gen would fix. The blocker is that moving is only safe with **precise**
JIT roots, which the shadow stack does not yet fully provide.

## Design

The moving young gen can become the default **only** when, for every live JIT
frame, the GC has a precise, **rewritable** description of every register- and
stack-resident oop. Two routes; they converge on the same metadata as
`real-frame-deopt.md`.

### Route A — finish the shadow stack into a rewritable precise map (lower risk)

Today the shadow stack pins. To *move*, every published oop slot must be
**rewritable** so the copy can relocate it and patch the slot:

1. Extend `shadow_stack.rs` so each entry records not just the oop value but its
   **home** (register id or native stack offset) and a way to write back the
   forwarded address — i.e. the shadow becomes a precise, updatable root list,
   not just a scan list.
2. After the semispace copy computes forwarding pointers, walk the shadow stack
   and rewrite each home with the forwarded address (registers via the saved
   register file at the safepoint; stack slots in place).
3. Preserve selective promotion semantics so the **bt18 = 68332206** invariant
   holds: the moving path must tenure the same node set the non-moving sweep +
   selective promotion does (the under-counting 67674804 came precisely from
   *not* pinning/promoting those nodes).

### Route B — reuse the deopt safepoint maps (shared infrastructure)

The per-safepoint register→slot maps designed in `real-frame-deopt.md` are
*exactly* the precise root description a moving collector needs. If GC only ever
moves at safepoints (it does — `gc/src/safepoint.rs`), the safepoint's
`FrameState` enumerates every live oop and its home. Build the moving young gen
on the deopt safepoint table:

1. At a GC safepoint, for each live JIT frame look up its `DeoptimizationPoint`
   by native PC, collect the `FrameValue::Register`/`StackSlot` oops.
2. Treat them as precise rewritable roots in the Cheney copy.
3. Patch them post-copy from the forwarding table.

Route B is more work up front (depends on deopt maps) but unifies precise-roots
infra; Route A is incremental on existing shadow-stack code. Recommend **A
first** (it can ship behind a gate and be validated against bt18 in isolation),
**migrate to B** once deopt maps exist.

### Correctness invariant harness

Whatever route: gate the new default behind a flag, and make CI assert
`bt18 == 68332206` (and bt10/14/16 vs `java -cp bench BenchSuite`) before the
flag flips to default-on. The selective-promotion default-on flip (`Fix A`)
already established this as the acceptance test; reuse it. The
`MEMORY.md` bintrees measurement-loop reference documents the run recipe (8g
heap, taskkill stray cratonvm first, `build-cpu.bat`).

## Implementation steps (ordered)

1. **Make the shadow stack precise+rewritable** (Route A): record home
   (reg/stack-off) per published oop in `shadow_stack.rs`; add a writeback API.
2. **Wire moving-with-shadow-rewrite** in `collect_garbage_inner`: when shadow
   coverage is complete for all live frames, run the Cheney copy *and* rewrite
   shadow homes from the forwarding table — instead of the
   pin-only/non-moving path at `gen_heap.rs:2267`.
3. **Match selective-promotion tenuring** in the moving path so the live node
   set is identical to the non-moving sweep's.
4. **Gate + validate**: `CRATONVM_MOVING_YOUNG` (default-off). CI asserts bt18 =
   68332206 and the bt10/14/16 checksums; measure throughput vs the non-moving
   default.
5. **Coverage fallback**: if any live frame lacks complete shadow coverage at a
   safepoint, fall back to the non-moving sweep for that cycle (never move with
   an incomplete map). Count fallbacks; the flag can only become default when
   fallbacks are ~0 on the app gauntlet.
6. **Flip default**; keep `CRATONVM_NONMOVING_YOUNG` as the opt-out escape hatch.
7. **(Later) migrate to Route B** once `real-frame-deopt.md` safepoint maps
   exist, retiring the bespoke shadow rewrite.

## Risks

- **Under-counting regression** (the 67674804 trap): any node the moving path
  fails to keep alive or promote that the non-moving sweep would have is a
  silent correctness bug. The bt18 invariant catches the canonical case; other
  apps need the coverage-fallback safety net.
- **Incomplete shadow coverage** moving anyway = heap corruption (relocating an
  object whose register-home isn't rewritten leaves a dangling raw pointer in a
  JIT spill slot). The fallback in step 5 is mandatory, not optional.
- **Conservative/precise mismatch**: mixing a conservatively-scanned slot with a
  precisely-rewritten one for the *same* object must be impossible — once a
  frame is "precise", it must be fully precise.
- **Throughput might not improve as much as hoped** if the sweep cost was
  partly card-table / marking rather than allocation; measure before declaring
  victory. The `MEMORY.md` bt18 "JIT helper overhead" entry warns that part of
  the historical bt18 gap was per-node JIT-helper cost, *not* GC — re-confirm
  the GC share of the current 23x.

## Effort

XL, and gated on either finishing the shadow stack (Route A, large) or on
`real-frame-deopt.md` safepoint maps (Route B, very large). Route A is the
pragmatic first landable; the correctness harness already exists.
