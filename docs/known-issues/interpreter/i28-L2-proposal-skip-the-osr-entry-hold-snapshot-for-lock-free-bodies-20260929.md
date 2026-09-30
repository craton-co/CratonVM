# Proposal: skip the OSR entry's hold snapshot for a body that names no taken lock

**Status: proposal — filed 2026-09-29 by interpreter round i1 wave 28, lane L2.**

## Problem, with evidence

Wave 28 made `jit_bridge::try_osr` take an `OsrEntryHolds` snapshot before
every OSR body runs
(`docs/internal/fixed-bugs/interpreter-L2-release-beyond-live-record-trusts-an-under-stated-record-FIXED-20260929.md`):
one pass over the live frame's locals, a tag read per slot and, per non-null
reference, a mark-word read (`MonitorTable::holds`), plus an `entry_count`
read and a small allocation for each object the thread owns. Its only reader
is `CompiledLocksOfAStash::release_beyond_live_record`, which runs only when
an abandoned frame of THIS body names a lock its code took (`relock ==
false`): the depths vector is empty otherwise and the function returns before
it reads the snapshot.

OSR entries are not rare. Since the RBC.6b lift a caught exception inside an
OSR'd loop is an exit plus a re-entry: the `OsrExcRateProbe` run that
`try_osr`'s comment cites measured `osr_entered=2280923`. Every one
of those entries now pays the pass, although most OSR bodies take no lock at
all.

## Design

1. Give `cratonvm_jit::CompiledMethod` a `names_taken_locks: bool`, set where
   the artifact's `deopt_points` are final (the single-pass driver and the IR
   lowerer's `resolve_monitors` publication, or lazily in a `OnceLock` next
   to the memoised `osr_exit_policy`): true when any point's frame state, or
   any scope of its caller chain, lists a monitor with `relock == false` and
   `lock_depth > 0`. Reason-9 points are deopt points too, so the pad frames
   `release_locks_of_own_pad_exits` claims are covered, and so are the chain
   frames the chain refusal pins.
2. In `try_osr`, take the snapshot only when `compiled.names_taken_locks`;
   otherwise pass `OsrEntryHolds::default()` (the release returns 0 before
   reading it for such a body, so nothing changes).

## Expected win and how to measure it

Removes the pass from every entry of a lock-free body: on the order of one
tag read per local and one header load per reference local, against an entry
that already builds the seeded locals and tags and asks `validate_osr_entry`.
Expected well under 5% of an entry, so worth landing only if a measurement
shows it above the host's floor. Measure: a `*Bench*` timing probe of a
`try { a[i & 7] } catch (ArrayIndexOutOfBoundsException e) { ... }` loop
that throws on every eighth iteration (an OSR re-entry per catch), fat LTO,
JIT on, interleaved with the wave-28 landing; the row to compare is ns per
iteration, expected direction down. Positive control: `CRATONVM_DBG_JITC=1`
plus a counter of skipped snapshots, which must be non-zero on that probe and
zero on `L2W27TrySyncLoopLockAbandon` (a body that names its lock).

## Cost and risk

One bool on `CompiledMethod` (lane L6's `jit/src/lib.rs`) and one branch per
entry. The risk is a producer that publishes a taken lock outside
`deopt_points` (none today: every stash is reconstructed from a point's
frame state); a debug assertion in `release_beyond_live_record`
(`!self.depths.is_empty()` implies the snapshot was taken) would catch it.

## Staged plan

1. The field, computed lazily, and a unit test over a hand-built point list
   (a taken lock in a caller scope sets it; an elided one alone does not).
2. The gate in `try_osr` and the skip counter.
3. The timing probe, measured on the host before landing stage 2.
