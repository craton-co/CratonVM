# Proposal: a short cooperative first slice before a collection's round-0 take-over

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 35
> of 54).** Not built, unmeasured. On d7 take-over pauses are the common case
> (e.g. og3_steady_dflt_1 `takeover_pauses=163`), so the slice would be paid
> often. **Gate:** time-to-safepoint p50/p99 with the slice on and off on
> `GenR4W4EvacThroughputProbe`, `BinTreesClassic 18` and the netty suites;
> rejected if p99 grows. **Size:** S-M.

*Filed 2026-09-28 by the GC defects round orchestrator, from JIT round 13
wave 8 (`docs/internal/fixed-bugs/r13w8-mega7-grace-at-handshake-pause-patch-CLOSED-20260929.md`,
its patch 2). A direction, not a defect.*

- **Status:** PROPOSAL (unmeasured).
- **Backend:** every collector that uses `stw_take_over_and_wait`
  (`vm/src/runtime/interpreter/gc_and_alloc.rs`).

## Where things stand

`stw_take_over_and_wait` takes over (freezes) every counted peer on round 0
whenever `any_thread_in_jit()`. A compiled thread spinning in a loop is
frozen before it reaches its safepoint poll, so the pause never finds every
mutator at a poll. Two things only a cooperative stop allows are then skipped
on almost every collection:

- the megamorphic inline-cache grace (`cratonvm_jit::note_grace_at_cooperative_stop`).
  Patch 1 of the JIT page now also advances it at the non-collection
  handshake pause (`NonMovingPause::request_as`), but collections still skip it;
- precise roots for the frozen frame. Its words are read conservatively,
  which is the root of several `xt-helper-window-conservative-scan` fallbacks.

## Proposal

Before round 0, wait a bounded cooperative slice for peers to reach a poll,
as HotSpot's safepoint protocol does. The slice would be a few tens of
microseconds, tunable, and zero under a flag. Take over only the peers still
running when the slice expires.

## What decides it

- Pause-latency A/B on `GenR4W4EvacThroughputProbe`, `BinTreesClassic 18` and
  the netty ByteBuf suites: time-to-safepoint p50 and p99, with the slice on
  and off.
- The fallback census (`[moving-young] fallback ... reason=`) and the
  megamorphic reclaim counter under a spinning compiled thread.

A slice that costs more than it saves on the p99 pause is rejected.
