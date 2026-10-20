> **STATUS (2026-10-19, gcj j3/rf3): OPEN -- found by reading while building the legacy-seed half of ec47 (`CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED`, commit `db1726a4ab`); parity only (the objects kept are right, the depth-2 set is short), no row shows it yet.**

# ec47 residuals: a promoted member of the young resurrection set, and one inside an unparseable young stretch, still mark their old referents strongly

## Context

ec47 (`docs/internal/gc/gcj-j1rf-gen-major-roots-the-old-children-of-young-finalizables-FIXED-20261020.md`):
HotSpot clears a weak reference to an object reachable only through a dead finalizable at the
collection that finds the finalizable dead, before `finalize()`. CratonVM Generational now does
that on the true-root seed (gcj j2/rf2) and on the legacy young seed for YOUNG members of the young
phase's resurrection set (gcj j3/rf3: `mark_young_to_old_refs_deferring` moves their old slot values
into `OldGenFinalizerPass::deferred_seeds`).

## What is left (file:line on `c7503ea6e1`)

1. **A member the young phase PROMOTED.** `young_resurrection_members` (`gc/src/gen_heap.rs` ~18339, next to
   `take_resurrection_closure`) maps the recorded keys through `pointer_map`; a promoted member's
   current address is OLD, so the young walk never meets it. On the legacy path that object is a
   root of the major anyway when it is a promotion destination of a moving cycle (Phase 5's tail,
   `promotion-dest(moving)` in the census label, `collect_garbage_inner_with_pins` ~26266) or a
   finalizable (`finalizers_in_old`, deferred by rf2's X1 only for the finalizable itself), so its
   old children are strongly marked. A non-finalizable promoted member (a child of the dead
   finalizable that was old enough to tenure) keeps the defect on the legacy path.
2. **A member inside an unparseable young stretch.** The anomaly arm of
   `mark_young_to_old_refs_deferring` (~36829) scans the stretch conservatively and marks every
   old base it finds; it cannot tell which words belong to a member. Deliberately unchanged
   (soundness first), rare (a walk desync).

## What would close it

1. Pass the promoted members (`young_resurrected`'s old addresses) to the major as deferred seeds
   too and take them out of the moving tail's root slice (cross-lane: Phase 5's tail builder), the
   same move X1 made for `finalizers_in_old`. Sound for the same reason (the round always marks a
   deferred seed).
2. Nothing for the stretch arm short of a parse; leave it.

## How to verify

`Gcj1RfFinalizableOldChildWeak old` with `-XX:MaxTenuringThreshold=0` already promotes the
children before the finalizable dies; a variant whose finalizable and child are young together and
tenure in the resurrection drain (child age = threshold - 1 at the pause) would show item 1 on the
legacy arm (`CRATONVM_GC_FULL_GC_TRUE_ROOTS=0`): HotSpot `fact cleared-at-gc true`, CratonVM `false`.
