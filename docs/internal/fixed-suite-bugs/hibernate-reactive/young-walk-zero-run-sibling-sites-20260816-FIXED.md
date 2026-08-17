# The young walk treats a run of EMPTY objects as corruption at other sites

**Status: FIXED / CLOSED (2026-08-16).** All eight `zero_run_end` sites were
resolved in waves 1–7 (2026-08-13). This revision closes the two residuals that
kept the page open: the phantom-extent finding is now **explained and removed
by construction** rather than left as an unreproducible observation, and the
16-bytes-per-empty-object retention deferral is now a **ratchet in the binary**
rather than a remembered opinion. Retired from
`known-issues/hibernate-reactive/` on 2026-08-17.

Waves 1–7 and the site-by-site history are in the previous revision of this
page; what follows is only the two residuals and how they closed.

## Residual 1 — phantom extents under memory pressure

### What it was

`SWEEP_PHANTOM_EXTENTS` is a corruption guard in the young non-moving sweep. It
reads `side_sorted` — the cycle's mark set — as "live object BASES, and live
objects never nest", so a mark strictly inside a header's claimed extent proves
the walk left the object grid. The same predicate gates the parallel sweep's
`chunk_bail(5)`.

On 2026-08-13, under memory pressure, it fired hundreds of times per run with
`live_in_dead=0` throughout — nothing was ever freed wrongly, the sweep simply
threw away its own work — and the parallel sweep collapsed alongside it
(`par_accepts` 8/8 → 3/13 → 2/20, every chunk bail `phantom`). Twenty-five
re-runs could not reproduce it, **including at the commit it was recorded at**,
so the previous revision landed a discriminator (`SWEEP_PHANTOM_INTERIOR_MARKS`)
instead of a fix and left the verdict open: *equal counts say the guard is
firing on interior marks; zero says the walk really did leave the grid.*

### The premise was only ever assumed, and it is false

The leading hypothesis was right, and it does not need the workload to prove
it — it is decidable by reading the two sites and driving them directly.

`mark_young` side-marks **every** conservative candidate (the Family-A fix of
2026-07-03: no candidate is ever header-written). When the anchor oracle cannot
place a candidate — its interval's chain failed to land on the upper anchor —
the fallback marks the **RAW candidate address**:

```rust
None => {
    oracle_unresolved.borrow_mut().push(addr);
    (addr, ptr)                 // <- marked at the raw address
}
```

A conservative candidate is frequently an object-INTERIOR word: a field
address, a derived pointer, a spilled register mid-object. The late
base-resolution pass then walks the arena's own object grid, proves the
covering base, and marks **that** — but it never retired the raw mark. So
`side_sorted` carried both, and an interior mark inside the very object being
sized satisfies the guard's premise exactly.

Three unit tests establish this without the pressure regime:

* `an_interior_mark_makes_the_phantom_guard_condemn_a_valid_object` — the
  predicate fires on a single valid 128-byte object whose mark set is
  `[base, base+32]`; with only true bases it does not; and it still fires on a
  real phantom whose extent swallows the next object's base. (`+32` is the
  `victim_interior_offset` from the field report.)
* `resolve_candidate_bases_reports_proven_interior_candidates` — over a real
  three-object young arena, the grid walk separates the candidates that ARE
  bases from the ones that are strictly inside an object.
* `unmarking_a_proven_interior_address_leaves_the_bases_marked` — the bitmap
  half.

The predicate itself was extracted into `mark_strictly_inside` and is now
shared by the sequential walk and `sweep_chunk`, which had two copies of it.

### The fix

`resolve_candidate_bases` now returns a third list: the candidates it PROVED
are object-interior, by the same linear chain the sweep itself walks. The late
resolution pass clears their raw side marks (`YoungMarkBits::unmark`,
`LATE_RESOLVE_RAW_INTERIOR_CLEARED`).

This is neutral for retention and is not a weakening of the guard:

* an interior mark retained **nothing** to begin with — the sweep matches marks
  against object STARTS, which is the whole reason the late-resolution pass
  exists;
* the covering base was marked by that same pass, so the object is retained
  either way;
* the address is provably not an object start under the grid the sweep is about
  to walk, so removing it makes the guard's premise TRUE BY CONSTRUCTION rather
  than by assumption.

That is why this closes the residual whichever way the original observation
would have gone. If it was interior marks, it is fixed. If it was not, the
guard's premise now actually holds, so the next occurrence is unambiguous
corruption instead of a verdict that has to be re-litigated. The discriminator
branch "equal counts say the guard is firing on interior marks" can no longer
happen, because the mark is gone before `side_sorted` is materialised.

Screening interior marks *inside the check* — the tempting one-line change the
previous revision refused — is still refused, and correctly: it would cost
detection for a real phantom whose only subsumed mark happened to be an
unresolved candidate. Retiring the mark at its producer costs nothing.

### Measured on the workload

`BatchingConnectionTest` under `-XX:+UseGenerationalGC` with
`CRATONVM_GC_STATS=1`, six heap sizes, Azure Linux, `ok=61 failed=0` every run:

| `-Xmx` | par_attempts / accepts | phantom_extents | nonbase_marks | raw_interior_cleared | live_in_dead | chunk bails |
|---|---|---|---|---|---|---|
| 700m | 6 / 6 | 0 | 0 | 0 | 0 | all 0 |
| 450m | 9 / 9 | 0 | 0 | 0 | 0 | all 0 |
| 320m | 15 / 15 | 0 | 0 | 0 | 0 | all 0 |
| 260m | 21 / 21 | 0 | 0 | 0 | 0 | all 0 |
| 220m | 21 / 21 | 0 | 0 | 0 | 0 | all 0 |
| 190m | 27 / 27 | 0 | 0 | 0 | 0 | all 0 |

`raw_interior_cleared=0` is worth stating plainly rather than reading as
success: on a quiet host the oracle resolves every candidate, so the producer
never fires and this workload never exercised the path either way. That is
consistent with 25 runs failing to reproduce the original finding, and it is
why the closing argument is the code fact and the unit tests, not this table.
What the table does establish is the absence of a regression:
`par_accepts == par_attempts` at every size, every guard zero.

## Residual 2 — the 16-bytes-per-empty-object retention

An accepted empty-object run is stepped over, not parsed, so each dead empty
object in it stays until a moving cycle resets from-space — and under a
permanent non-moving sweep there is no moving cycle.

The measurement said it does not accumulate: flat at 11.5–29 KB from 4 young
collections to 27, ceiling 0.045% of the young generation, because every cycle
re-skips the SAME runs rather than adding new ones. The decision that followed —
keep stepping over rather than reclaim, because pushing the run as a dead region
would be the first change in this family to FREE something the previous code
retained — was correct on those numbers.

It was also, as the previous revision said in as many words, a decision that
would have to be "re-checkable rather than a remembered opinion". It now is one.

`empty_run_retention_exceeded` scores the standing retention against a budget
of 10 permille (1%) of young `used`, with a 64 KiB floor so a tiny young
generation cannot produce a trend reading out of arithmetic.
`EMPTY_RUN_RETENTION_EXCEEDED` counts the sweeps that fail it and a bounded
`gc::guard` warning says so. Two loads and a compare, once per sweep,
unconditional — the whole point is that it runs on workloads nobody thought to
re-measure.

`the_empty_run_retention_ratchet_is_silent_on_every_measured_figure` encodes all
thirteen measured `(retained, young_used)` pairs from both rounds plus
`CascadeComplicatedTest`, and asserts the ratchet fires on growth and not on
them. The budget's ~22x headroom over the measured ceiling is deliberate: it is
a trend detector, not a tuning knob.

Re-measured on this branch, same class, same flag, six heap sizes on Azure
Linux:

| `-Xmx` | retained | young used | fraction |
|---|---|---|---|
| 700m | 16 352 B | 139.1 MB | 0.012% |
| 450m | 14 464 B | 102.1 MB | 0.014% |
| 320m | 24 256 B | 83.8 MB | 0.029% |
| 260m | 19 840 B | 68.1 MB | 0.029% |
| 220m | 23 968 B | 57.6 MB | 0.042% |
| 190m | 19 072 B | 49.6 MB | 0.038% |

14.4–24.3 KB against 11.5–29 KB before, ceiling 0.042% against 0.045%. No
growth, three rounds of measurement apart, and the binary now says so itself if
that ever stops being true. **The trade stands, and it is no longer a decision
anyone has to remember.**

## Instruments

`CRATONVM_GC_STATS=1` prints, per run:

```
[GC] young_sweep: par_attempts= par_accepts= zero_spans= zero_empty_runs=
     phantom_extents= phantom_nonbase_marks= raw_interior_cleared=
     live_in_dead= walk_overshoot= anchor_not_a_base=
[GC] young_sweep_empty_runs: last_cycle_bytes= young_used=
[GC] young_sweep_chunk_bails: overshoot= gap_filler= zero_span= bad_size=
     hole_crossing= phantom= anchor_miss=
```

`raw_interior_cleared` is the producer side of `phantom_nonbase_marks`: the two
can no longer both be non-zero for the same address, because the mark is
retired before `side_sorted` exists.

## Related

- `young-sweep-empty-object-run-unwind-20260812-FIXED.md` — wave 1–4, the
  sites this page grew out of.
- `asynchronousfilechannel-close-waits-for-the-read-it-cancels-20260816-FIXED.md`
  — retired in the same pass; the GC probes above run on its fixture.
