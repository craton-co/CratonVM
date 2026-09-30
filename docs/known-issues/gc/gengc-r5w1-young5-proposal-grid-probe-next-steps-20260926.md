# Proposal: the next steps now that every young walk shares one grid probe

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 4
> of 54).** Not built. Re-read at `307f0c6a2`: P3 is still latent in
> `gc/src/gen_heap.rs::store_object_starts_locked`, which stores the new
> bitmap pointer and then drops the previous `Arc` in the same call (`hold[i]
> = bits`); any lock-free reader that loaded the old pointer before the store
> reads freed memory. Latent only because the writer runs inside a pause. P1,
> P2 and P4 are anomaly-path only. P3 goes first: it is a use-after-free
> shape, the others are hygiene. **Gate:** P3: a unit test that publishes
> twice and reads through a pointer loaded before the second publish (under
> Miri or ASan where available); P1/P2: the page's straddle fixture tests,
> `LATE_WALK_ZERO_RUNS` unchanged. **Size:** S (P3), S (P1+P2), M (P4).

Slug: `gengc-r5w1-young5-proposal-grid-probe-next-steps`
Filed 2026-09-26 by generational GC round 5, wave 1, lane `young5`.
**Status: PROPOSAL.** Follows
`../../internal/gc/gengc-r4-sweep-young-linear-walk-variants-drift-FIXED-20260927.md`, whose rules are
now one function (`gen_heap::probe_grid_at`) with a per-walk `GridRules`.

With the rules in one place, the remaining differences between walks are
visible as `GridRules` fields and match arms, which makes these small and
reviewable. Each one changes a decision on an ANOMALY path only.

## P1: the three moving-path walks take the hole-crossing verdict

`mark_young_to_old_refs`, `fixup_young_old_refs` and `walk_young_objects`
still parse a header whose extent crosses the next free block as an object
(their match arms map `CrossesFreeBlock` to "object", on purpose, to keep
this wave decision-identical). Parsing it reads the free block's stale bytes
as reference slots and strides past the hole, off the grid. Proposed arms:

- `mark_young_to_old_refs`: `CrossesFreeBlock { free_off, .. }` → the same
  conservative old-base scan its anomaly arm runs, over `[cursor, free_off)`,
  then `cursor = free_off`.
- `fixup_young_old_refs`: `rewrite_stretch_conservatively(cursor, free_off)`,
  then `cursor = free_off`.
- `walk_young_objects`: `cursor = free_off` (the object is not emitted; its
  callers' roots are over-approximated by the conservative arms above).

The first two are strictly safer than today (a conservative scan over the
stretch instead of a parse through a bogus header). Unreachable on a healthy
arena. Verify: a unit test per walk on the drift page's straddle fixture
(`gen_r5w1_young5::Fixture`'s X), and `LATE_WALK_ZERO_RUNS` unchanged on
`probes/GcWalkProbe.java`.

## P2: `gen_heap_oldmark_census.rs` onto the probe

Its young walk strides the GAP filler by hand (line ~291) and has no zero-run
or hole rule. It is a census, so `YoungGridCursor` (re-anchor and continue)
is the right policy. Diagnostic only.

## P3: the per-heap object-start mirror as a snapshot

`GenerationalHeap::object_starts` publishes an `Arc<HeapBitmap>` pointer per
slot and DROPS the previous `Arc` right after storing the new pointer
(`store_object_starts_locked`), the same retire-then-drop shape the commit
mirror had. A reader that loaded the old pointer before the store reads freed
memory after the drop. Latent for the same reason (the one writer runs inside
a pause; readers are mutators), but unlike the commit maps these bitmaps are
LARGE (one bit per 8 bytes of arena), so the commit screen's "keep every
snapshot until the heap drops" answer would retain a grown arena's old
bitmap. Proposed: keep only the previous generation's two `Arc`s in a
`retired` slot, freed at the NEXT publish (one pause later), which closes any
reader that started before the publish and finished within a pause of it —
every reader, since readers cannot overlap the pause that frees.

## P4: a `Phantom` arm for the selective-promotion evacuation walk

The evacuation pre-pass runs without the phantom check; the main sweep walk
that follows has it. A header whose extent subsumes a marked base is not
promoted by the pre-pass today (the phantom's own address is not marked), but
its stride desyncs every decision up to the next anchor, which the pre-pass
then promotes on. `GridRules { phantom: true, .. }` and an unwind arm with its
own `EVAC_UNWIND_REASONS` slot would make the two walks agree on where the
grid is broken. Needs the unresolved-mark set (as the sweep walk has) to avoid
refusing on interior raw marks.
