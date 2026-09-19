# Moving / compacting young generation

**Status:** Shipped (default on; `CRATONVM_NO_MOVING_YOUNG=1` opts out).
The remaining scope — widening it to cover multi-threaded phases for
throughput — is **not planned**: the premise it was scoped against was
measured and refuted (see [Why the remaining scope is not
planned](#why-the-remaining-scope-is-not-planned)).

## What it does today

The young generation is a semispace Cheney copy, and it is the compiled-in
default: `DEFAULT_MOVING_YOUNG = true` in `types/src/flags.rs`. The flag is
shaped as an **opt-out** — `CRATONVM_NO_MOVING_YOUNG` disables it, and the
older `CRATONVM_MOVING_YOUNG` opt-in survives only as a compatibility no-op.
Both shapes are pinned by tests in that file.

**The flag being on is not the same claim as "this cycle compacted."**
`collect_garbage_inner` (`gc/src/gen_heap.rs`) computes

    moving_young = moving_young_requested && !divert_for_incomplete_moving_coverage

and diverts to `run_non_moving_young_cycle` whenever the per-cycle rewritable
root-coverage proof is incomplete, counting a fallback with a reason.
`gc/src/gc_quiescence.rs` holds the reason histogram. This is fail-closed by
construction: relocating an object whose register home cannot be rewritten
would leave a dangling pointer in a JIT spill slot, so an unproven cycle
sweeps instead of copying.

Ask `CRATONVM_DBG=gc-stats` for
`[GC] moving_young: cycles=… coverage_fallbacks=…` plus the per-reason
histogram before treating the collector as actively compacting in a given run.
A correct checksum proves safety; only a non-zero cycle count proves copying.

## What is not built yet

- **The cross-thread coverage handshake.** A cycle is treated as unproven
  whenever a peer thread is in compiled code, so multi-threaded phases keep
  taking the non-moving sweep. It is counted as `cross-thread-jit-peer` in the
  fallback histogram.
- **Route B** below — rebuilding the precise root description on the deopt
  safepoint maps instead of the bespoke shadow rewrite. Route A shipped; the
  migration was never needed.

## Why the remaining scope is not planned

This feature was originally scoped as a throughput lever: make the young gen
compacting to close the Binary-Trees-18 gap against HotSpot. **That premise did
not survive measurement.** The garbage collector is a negligible share of the
bintrees-18 gap, so widening moving-young's coverage buys approximately nothing
on the workload it was justified by. On bintrees-18 the compacting path is in
fact the *slower* of the two once the live set is large — a copying collector's
worst case.

What it does buy, and the reason it stays on, is **footprint**: the non-moving
path dies with `OutOfMemoryError: young gen exhausted` on heaps where the
compacting collector completes. Coverage fallbacks are the safety mechanism
working as designed, not a reason to disable the default.

## Design rationale

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

