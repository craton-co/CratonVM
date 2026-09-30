# The parallel young evacuator's scaling limits (by reading) and what to change

> **STATUS (2026-09-29, gce e2/y): OPEN (perf) -- decisive rows written.** The `evac_nojit_*` rows timed out at the 300 s edge under load on both binaries.
> - **e1 numbers** (orchestrator, 3 runs): `--nojit` 300 (timeout)/166/194 s against 240/207/173 on the base; with `CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS` 255/194/164 against 273/239/176.
> - **Replacement rows:** the short arm `GenR4W4EvacThroughputProbe 262144 4000000` (`evac_short_nojit_*`, `evac_short_nojit_keep_*`, section 3 of `docs/internal/gc-design-perf-round-20260929/e2-y-report.md`). Stdout SAME (`checksum=6471610074841698382`); compare `[evac-timing] steady_median_ms` medians.
> - **Accept** `CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS` as a default candidate when its median is below the plain arm's by more than the plain arm's own spread.

> **STATUS (2026-09-29, gce e1/x): KEEP -- perf page; the ABBA medians are unread.** `evac_nojit_1..3` and `evac_nojit_keep_1..3` (`verify-e1/ve1`) end rc 0 except `evac_nojit_1` on e1 (rc 124); the battery's `w3_evac_nojit` timed out on base and finished on e1, so these timeouts are the host's load on both binaries. **Remaining:** `evac_drain` and `epilogue` medians with and without `CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS=1`, and the `map_merge` phase now on the pool.

> **STATUS (2026-09-29, gce e1/y): OPEN (perf) -- the default-path list worked through: one item landed opt-in, one written as a cross-lane hunk, one judged not worth it; plus two default-on fixes on the sweep arm (the default JIT-warm cycle).** Unbuilt when written.
>
> 1. **`map_merge` on the heap's pool: written, cross-lane (types).** `PointerMap::par_extend_pairs` (`types/src/pointer_map.rs`) spawns `threads - 1` fresh OS threads per moving cycle. The hunk (request 2 of `docs/internal/gc-design-perf-round-20260929/e1-y-report.md`) adds `PointerMap::par_extend_pairs_with(sources, threads, run)`: the caller's runner executes one job per shard chunk (a per-chunk `Mutex` hands each job its `&mut` slice safely; a job the runner skips is folded afterwards on the calling thread, so a narrower pool costs time, never entries), and the driver in `collect_garbage_inner_with_pins` passes `EvacPool::scope`. Not landed in gc, because the gc half does not compile without the types half.
> 2. **Decommit hysteresis: LANDED opt-in, `CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS`** (read per moving cycle through `runtime_flag_on`; default off, so the give-back is byte for byte as before). The evacuated semi-space is the next cycle's to-space and the copy bumps it from 0, so every survivor the next copy writes first faults a released granule in INSIDE the pause. With the switch the give-back keeps `young survivors x 1.25` committed (`uncommit_keep_for_survivors`, `uncommit_evacuated_young_keeping`), on top of the `-Xms` floor and wiped off-pause like it. Cost: that prefix stays committed and the wipe thread zeroes it instead of the kernel.
> 3. **W2's card seed default flip:** unchanged, triage (the d8 table marks `CRATONVM_GC_PAR_EVAC_CARD_SEED` as riding with the pinned copy).
> 4. **W3 / rest of W5:** not worth doing on the default path: W3 needs the allocator (lane o) to stop short of capacity, and a bufferless plan happens only on allocation-failure cycles; the shard reuse saves a few `Vec` reallocations per worker per cycle once `prev_survivor_count` pre-sizes `forwards` (already landed).
> 5. **The sweep arm (default on, behaviour-identical):** the mark drains and the span zeroing now run on this heap's persistent pool instead of spawning threads per call, and the card bulk re-mark skips repeats and already-dirty cards; see the gce e1/y STATUS of `gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926.md`.
>
> Tests: `cargo test -j 5 -p cratonvm-gc --lib gce_e1y_tests` (`e1y_the_keep_is_the_survivors_plus_a_quarter_and_only_when_opted_in`, `e1y_the_evacuated_semi_space_keeps_the_survivor_prefix_committed`); with request 2 applied, `cargo test -j 5 -p cratonvm-types --lib pointer_map`. Verify item 2 on MOVING cycles (`--nojit`): `CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 cratonvm --java-home "$JDK" --nojit -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe`, ABBA with and without `CRATONVM_GEN_UNCOMMIT_KEEP_SURVIVORS=1`: both print `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`; accept when the `evac_drain` + `epilogue` medians fall and `time_ms` is not worse. Request 2: the same run, `map_merge` medians no higher, `[GC] evac_pool:` dispatches up by one per moving cycle.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): unchanged -- OPEN (perf), no code change.** Single battery runs of `GenR4W4EvacThroughputProbe -Xmx256m` (checksum `1021046735439613382`, `corrupt=0` on every arm): JIT default 23140 ms (`evac_throughput`, with `CRATONVM_DBG=gcpause`) and 17853 ms (`w3_evac_default`); pinned copy + parallel + card seed 14052 ms (`w3_evac_pinned_par`); `--nojit` 111465 ms (`w3_evac_nojit`) against 99269 ms with the W1/W2/W4 opt-ins (`w3_evac_w124p`). One run per arm on this host is inside its ~3x noise, so nothing is decided; W3 and the rest of W5 remain the default-path work.

## STATUS (2026-09-28, gcd d5/s): OPEN (perf); no code change; what is still worth doing on the DEFAULT path

Re-read on `d916d1c40`. The parallel evacuator runs on the default path only
on a MOVING young cycle: JIT-cold or `--nojit` programs, and a JIT-warm
cycle whose term 4 did not fire. On the probe this page measures
(`GenR4W4EvacThroughputProbe`, JIT-warm) no default cycle copies, so W1-W5
cannot move its time; the d4 build ran it in 18-20 s with and without the
take-over flag, which engaged zero times
(`../../internal/gc/gcd-d5s-takeover-arm-sees-only-refusing-helper-windows-FIXED-20260928.md`).

Still worth doing, for the default moving cycle, in order of expected gain:

1. **`map_merge` (5-12 ms a cycle, by reading).** `par_extend_pairs`
   (`types/src/pointer_map.rs`, not a young-lane file) spawns fresh OS
   threads per cycle; reusing the evacuator's pool (`gc/src/evac_pool.rs`)
   is the contained fix, but it crosses into `types`, which cannot depend
   on `gc`. A `PointerMap::extend_pairs_with(&dyn Fn(..))` hook that `gc`
   drives from its own pool would keep the dependency direction.
2. **The per-cycle decommit and re-fault of the evacuated semi-space**
   (`CRATONVM_GEN_UNCOMMIT`, default on): a hysteresis (keep the semi-space
   committed while the last N cycles reused it) is proposed on the retired
   round-two page; it is a sizing policy, so opt-in first.
3. **W2's card seed on the workers** (`CRATONVM_GC_PAR_EVAC_CARD_SEED`,
   landed opt-in): the default-flip candidate once its A/B on BinT 14,
   HashMapOnly and `MtChurnProbe` (moving cycles) shows `card_root_forward`
   gone from the driver with `evac_drain` not worse.
4. W3 (allocator slack) and W5 (shards kept in the pool) remain small.

None was changed this wave: each is either another lane's file or a policy
flip that needs its A/B first. Verification commands unchanged (below).

## Previous STATUS (2026-09-26, gen r5w3/evac7): W1, W2 and W4 landed behind opt-in flags; the probe's breakdown by reading; the top three costs

**Landed, each default OFF, each changing no GC decision.**

| item | flag | where |
|---|---|---|
| W1: a share-publish wakes `min(idle, published)` workers (`notify_one` each), not `notify_all` | `CRATONVM_GC_PAR_EVAC_TARGETED_WAKE` | `gen_evac.rs` `ParEvac::run_worker` |
| W2: the dirty-card roots are forwarded on the workers (an equal slice each, inside `run_worker` after its exit guard), not on the driver before the pool opens | `CRATONVM_GC_PAR_EVAC_CARD_SEED` (ignored under `CRATONVM_GC_FULL_RSET_SCAN`) | `ParEvac::drain_seeded`; driver in `collect_garbage_inner`'s parallel arm |
| W4: each worker's first promotion buffer comes from the previous parallel cycle's promoted bytes ÷ workers, clamped 16..256 KiB | `CRATONVM_GC_PAR_EVAC_OLD_PLAB_HINT` | `EvacShard::seed_first_old_plab`; per-heap `prev_par_promoted_bytes` |
| referent-header prefetch: a bounded 8-element lookahead in reference arrays, all referents for instances, the next card root in the card seed; in every copy scan | `CRATONVM_GEN_EVAC_SCAN_PREFETCH` | `gen_heap::forward_ref_slots_pf`, `seed_dirty_card_roots`, `ParEvac::scan_object` |

Also landed in `gen_evac.rs`, behind `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL`:
the pinned in-place cycle on the parallel evacuator (`ParInPlace`).

**Changes that are live on the default path, all behaviour-neutral:**

- `run_worker` scans its local stack before it first acquires. Without a
  seed the local stack is empty on entry, so the operations run in the same
  order as before.
- `drain` is `drain_seeded(None)`.
- `seed_dirty_card_roots` takes the young test as a predicate.

W3 (allocator slack) and the rest of W5 (shards kept in the pool) remain open.

### Where a young cycle's time goes on `GenR4W4EvacThroughputProbe` (by reading; estimates, nothing run)

**The probe's shape.**

- Each iteration allocates 120 B: `Node` 24, `int[4]` 32, `byte[48]` 64.
- The run allocates 2.4 GB.
- The young semi-space is 64 MiB at `-Xmx256m`.
- The default 200 ms pause goal caps the trigger at the adaptive moving
  threshold, at most 32 MiB, on both arms.
- About 14-18 MB of ring nodes stay young, because the tenuring age is 3. That
  leaves about 14-18 MiB of allocation per cycle, so about 130-170 cycles per
  run. HotSpot runs about 34 over its 68 MB eden.

**1. On default flags, no young cycle on this probe copies.** Its loop is
compiled, and term 4 (`unrewritable_conservative_jit_roots`) sends every
cycle to the in-place sweep. Two separate breakdowns follow: that cycle, and
the moving cycle the probe gets with the pinned copy on or under `--nojit`.

**The non-moving cycle** (the default path;
`gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926.md`):

| cost | est. per cycle |
|---|---|
| mutator: mini-TLAB refills from 64-200 B holes, slow-path allocations, a second zeroing of every reclaimed byte | 20-60 ms of mutator time per epoch |
| two serial whole-from-space walks (selective-promotion walk, fixup 3a), ~1.3M headers each | 15-30 ms |
| two serial random passes over ~250k ring targets (card seed, 3c) | 10-25 ms |
| zeroing 40-55 MB | 5-10 ms |
| free-list collected, sorted and published twice | 3-8 ms |

**The moving cycle** (the parallel copy; 8 workers). This is the breakdown
the lane was asked for, per phase mark:

| phase (`[gcpause]` row) | what | est. |
|---|---|---|
| `card_root_forward` | **serial**: ~250k ring-held Nodes, half of all survivors, copied on the driver before `evac_drain`, each a random-access miss | 40-60 ms |
| (`epilogue` + mutator) | the evacuated semi-space decommitted every cycle (`CRATONVM_GEN_UNCOMMIT` on) and re-faulted: ~16 MB of to-space faults inside the pause, ~32 MB of TLAB first-touch faults after | 10-20 ms |
| `evac_drain` | ~500k objects on 8 workers; per object: start-bit test, snapshot, owned-header re-read, CAS claim + publish, `set_gc_age` RMW, two `Vec` pushes | 8-12 ms |
| `map_merge` | a serial pass over every pair (`note_object_start`), then `par_extend_pairs`, which spawns fresh OS threads per cycle (`types/src/pointer_map.rs`), one hash insert per survivor | 5-12 ms |
| after the pause (VM) | `update_all_roots`: one hash lookup per root slot (statics, strings, mirrors, JNI, frames) with no from-space range pre-filter | 3-10 ms |
| `pre_evacuate` (= the card scan) | ring walk, ~250k `extra_roots` pushes, map pre-size | 2-5 ms |
| `objstart_walk` | a full walk of used from-space, dead objects included (parallel) | 2-4 ms |
| `major_check` | ~250k deferred card re-dirties (serial) | 1-3 ms |

**The top three costs:**

1. **The divert.** JIT-warm cycles never copy: young fragments, allocation
   slows, and cycles come every ~14 MiB. The fix is the pinned in-place copy
   and its parallel arm (flags above).
2. **The sweep's serial O(allocated) passes** (non-moving) — or, when the
   cycle copies, **the serial card-root seed** (W2, landed behind
   `CRATONVM_GC_PAR_EVAC_CARD_SEED`).
3. **The per-cycle decommit and re-fault of the evacuated semi-space.**
   `CRATONVM_GEN_UNCOMMIT=0` is the existing A/B lever; a hysteresis is
   proposed in `../../internal/gc/gengc-r5w3-evac7-proposal-young-copy-round-two-RETIRED-20260927.md`.

**Probe commands.** The expected line on every arm is
`PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`.
Compare `time_ms` and the `[gcpause]` medians ABBA, interleaved.

```
cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
CRATONVM_GC_PAR_EVAC_TARGETED_WAKE=1 CRATONVM_GC_PAR_EVAC_OLD_PLAB_HINT=1 CRATONVM_GC_PAR_EVAC_CARD_SEED=1 CRATONVM_GEN_EVAC_SCAN_PREFETCH=1 \
  CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL=1 CRATONVM_GC_PAR_EVAC_CARD_SEED=1 \
  CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
```

- **Arm 2.** On default flags it changes nothing on this probe, which has no
  moving cycles to change. Read it on the moving cycles of BinT 14,
  HashMapOnly and MtChurnProbe, and with `--nojit`. W1: `evac_drain` medians
  and `helper_scans`, which must not fall.
- **Arm 3.** It makes the probe copy. The cycles must read
  `moving-pinned-pages` and `[GC] young_pinned_copy: pycopy_cycles` must
  move. This arm is blocked from a default flip by the netty data in
  `gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md`.

**Unit tests:**

```
cargo test -p cratonvm-gc --lib gen_evac::tests
cargo test -p cratonvm-gc --lib r4w5_pinned_young_copy
cargo test -p cratonvm-gc --lib young_mark::tests
```

- **New in `gen_evac::tests`:**
  `the_in_place_span_pool_claims_in_order_and_knows_what_it_wrote` and
  `the_first_promotion_buffer_seed_is_clamped`.
- **New in `r4w5_pinned_young_copy`:**
  `a_parallel_pinned_cycle_keeps_the_pinned_object_and_copies_the_rest`,
  `a_parallel_pinned_cycle_keeps_a_wide_graph_intact_across_two_cycles` (with
  W2 and the prefetch on) and
  `the_prefetching_forwarder_forwards_exactly_what_the_plain_one_does`.
- **New in `young_mark::tests`:**
  `zero_spans_parallel_splits_a_few_huge_spans_exactly`.
- The existing `a_worker_that_unwinds_does_not_strand_its_peers` exercises
  the reordered `run_worker`.

## Earlier status (2026-09-26, gen r5w1/young5; superseded by the block above): W5's reservation now reaches the helpers; two correctness fixes; W1-W4 proposed with code, not landed

Landed in `gc/src/gen_evac.rs`, all behaviour-neutral on the default path
except where a failure path was fixed:

- **W5, the part wave 6 thought it had landed.** The driver reserved every
  shard's `forwards` from the previous cycle's survivor count, but `drain`
  built each helper's shard fresh (`EvacShard::default()`) and then
  overwrote the caller's shard with it, so only the DRIVER's list kept its
  reservation; the helpers' reservations were allocated and freed inside the
  pause. `drain` now moves each caller shard into its helper's slot
  (`mem::take`) and the helper fills that one.
- **Fix (correctness, failure path): a descheduled claim winner.** The
  forwarded fast path and the CAS-loser arm read the target with
  `ObjectHeader::forwarding_address`, whose `FORWARDED|BUSY` wait is BOUNDED
  and answers NULL on timeout. On a proved object start that is a genuine
  claim, so the fast path returned the from-space address unrecorded (a
  dangling reference after the reset) and the loser arm recorded `(old, 0)`
  and returned NULL. Both now use `wait_for_claimed_forwarding`, which keeps
  asking and yields between bounded waits.
- **Fix (failure path): the non-forwarded CAS loser** now gives its abandoned
  copy the same treatment as the forwarded loser (verifier record,
  reference-free filler, `PAR_EVAC_ABANDONED_OLD_BYTES`), closing the old-gen
  liveness-blind dirty-card hazard on that arm.
- **Docs that lied:** `fetch_add` (the shared cursor is a CAS loop,
  `bump_shared`), the "8/16/24/32-byte" GAP-filler sentence, `PAR_EVAC_CYCLES`
  ("actually ran in parallel": it also counts a drain run inline with no
  helper), and in `evac_pool.rs` the `plan`-narrows-width claim, the
  "`ensure_helpers` has no production caller" claim, and the "benign"
  dispatch-while-busy arm (it is not benign; the `dispatch` mutex is what
  makes it unreachable). A `debug_assert!` after the job's publication in
  `EvacPool::scope` was removed (it was the pattern its own comment argues
  against).

Not landed: W1 (targeted wake-up instead of `notify_all` per share-publish),
W4 (first promotion buffer from the previous cycle), and the serial scan
prefetch. Each changes timing on the hottest loop and so must be opt-in, and a
new flag needs four registry files, two of them generated with enforced
counts that this lane could not regenerate. Code and registry diffs:
`docs/internal/gc/gengc-r5w1-young5-proposal-evacuator-contained-wins-DONE-20260928.md`.
W2 (parallel seeding) and W3 (allocator slack) are unchanged.

**Probes.**

```
cargo test -p cratonvm-gc --lib gen_evac::tests
cargo test -p cratonvm-gc --lib evac_pool::tests
```
all pass (the CAS-loser tests exercise the changed arm; the pool tests the
changed dispatch).

```
CRATONVM_DBG=gcpause cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe
```
prints `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`;
`evac_drain` / `map_merge` medians no higher than the base binary's (ABBA,
interleaved); at `CRATONVM_GC_PAR_THREADS=16` the helpers now keep their
reservation, so `evac_drain` may fall. Also on an oversubscribed host (run it
beside a `cargo build -j 5`): the same PASS line, and
`FORWARDING_BUSY_WAIT_ABANDONED` (if printed) no longer coincides with a
corrupt reference.

---

*Filed 2026-09-24 by generational GC round 4, wave 4, lane `young4`.*
*Status: **open (proposal)**; one contained fix landed with this page (§6).
Wave 6 (`young6`): the pool half of §1/§5 and proposal W5 **FIX LANDED,
awaiting probe:** `CRATONVM_DBG=gcpause cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4EvacThroughputProbe`
must still print `PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`,
with `evac_drain` medians no higher (and at `CRATONVM_GC_PAR_THREADS=16`,
lower) than the base binary's; W1-W4 are cross-lane (see the wave-6 section).*
*Severity: **perf** (young pause on multi-core hosts with a young generation
past `CRATONVM_GC_PAR_MIN_BYTES`).*

Scope: `gc/src/gen_evac.rs` (`ParEvac::{plan, evacuate, drain, run_worker,
plab_alloc, promote_alloc}`), `gc/src/evac_pool.rs`, and the driver in
`gen_heap::collect_garbage_inner`'s parallel arm. Nothing below was measured
in this lane (no cargo, no VM); every limit is argued from the code, and each
proposal names the counter or phase mark that would confirm it.

## 1. Work distribution — one global stack behind one mutex

`run_worker` acquires from `DrainState::stack` (a `Vec` under
`parking_lot::Mutex`) in shares of `len.div_ceil(threads).clamp(1, 256)`, and
publishes half its local stack whenever `idle_hint > 0` (`SHARE_MIN = 8`) or
all but `SPILL_KEEP` above `SPILL_HIGH`. Every acquisition and every
publication takes the same lock, and every publication does `notify_all`.

* With the measured sharing rule (a publish per ~8 local entries while anyone
  is idle), a narrow-frontier graph turns into lock traffic proportional to
  the number of hand-offs, and `notify_all` wakes every idle worker to contend
  for one lock to take one share. At 8 workers (the policy's usual width) the
  2026-09-02 numbers (`SHARE_MIN` table in `gen_evac.rs`) show it is fine;
  at 16-64 workers it is the first thing that stops scaling.
* **Proposal W1:** per-worker work-stealing deques (Chase-Lev; no new crate
  needed — a bounded array deque with an atomic top/bottom is ~150 lines, or
  `crossbeam-deque` if the workspace accepts it), owner push/pop at the
  bottom, thieves steal half from the top; termination by the existing
  idle-count protocol. `notify_one` per publish instead of `notify_all`.
  Confirm with `PAR_EVAC_HELPER_SCANS` (must not fall) and `evac_drain`
  medians at `CRATONVM_GC_PAR_THREADS=16/32`.

## 2. The seeding phase is serial

The driver forwards every root, overlay root and dirty-card root
(`seed_roots`, `seed_overlay_roots`, `seed_dirty_card_roots` into shard 0)
before `drain` opens the pool. Those phases (`root_forward`,
`overlay_forward`, `card_root_forward`) copy the objects they reach one at a
time on one thread. On a server with a large root set or many dirty cards
they are the serial fraction Amdahl charges.

* **Proposal W2:** hand root and dirty-card ranges out as tasks (the pool's
  `scope` already takes an index), each worker forwarding a slice into its own
  shard. `evacuate` is already safe for concurrent seeding (CAS claim).
  Confirm with the three seed phase marks.

## 3. To-space allocation — a shared cursor, bufferless when slack is thin

`plan` budgets `from_used` (100 % survival) and gives buffers only the slack
above it. Under the default 50 % young trigger the slack is ~half of to-space
and buffers are affordable; on a cycle triggered by allocation failure
(from-space ~99.9 % full — the bt18 measurement in `plan`'s doc) the cycle
runs BUFFERLESS, i.e. one `compare_exchange_weak` on one shared cache line per
copied object from every worker. That is the evacuator's worst scaling case.

* **Proposal W3:** reserve evacuation slack in the ALLOCATION policy: stop
  young allocation `workers * PLAB_MIN_BYTES * 2` short of capacity (the arena
  already has a high-end reserve mechanism, `remaining_high_reserve`), so even
  an allocation-failure cycle has buffer slack. Cost: that much young
  capacity (~128 KiB at 8 workers). Allocation-lane change; confirm with a
  census of bufferless cycles (count `plan.plab_bytes == 0` — a new counter
  beside `PAR_EVAC_DECLINED_SLACK`).

## 4. Promotion — the old-gen mutex per buffer

`promote_alloc` takes the old-gen mutex once per promotion buffer (16 KiB
doubling to 256 KiB) and once per object of 32 KiB and up; `retire_plab`
takes it once per worker at the end. That is amortised well; the residual is
a promotion-heavy cycle (promote-on-pressure, `force_promote_all`) at many
workers, where all workers start with a 16 KiB buffer at once.

* **Proposal W4:** seed each worker's first promotion buffer size from the
  previous cycle's per-worker promoted bytes (`tally[0] / workers`), so a
  promotion-heavy cycle carves one right-sized buffer instead of five
  doublings. Confirm with an old-gen lock-acquisition count.

## 5. Termination

"Every worker idle and the shared stack empty", counted under the mutex, with
`WorkerExit` counting unwound workers (round 4 `move`). Correct; its cost is
one lock round-trip per idle transition plus the `notify_all` herd of §1. W1
removes both.

## 6. The forward list recorded every re-encounter — FIXED in this lane

`evacuate`'s already-forwarded fast path pushed `(old, new)` on the shard's
`forwards` for every EDGE into from-space, not every object. The driver then
iterated all of them serially (the `note_object_start` anchor loop) and handed
all of them to `par_extend_pairs` (a hash insert each). For a to-space target
that pair is a duplicate: to-space starts every moving cycle empty and only
this phase's CAS winners install forwards into it, each pushing its own pair.
The fast path now pushes only for an OLD-gen target (which may be a
selective-promotion forward from an earlier non-moving cycle — the only
record). Same rule the serial evacuator adopted in round 4 `move`.
`tally[4]` still counts every re-encounter.

Test: `gen_evac::tests::an_already_forwarded_object_still_records_its_forward`
(both halves: old-gen target recorded, to-space target not);
`a_shared_child_converges_on_one_copy` (500 parents, one child) still asserts
convergence end to end. Confirm with `map_merge` and `evac_drain` medians on
`tools/bench/GenR4W4EvacThroughputProbe` (default args, `-Xmx256m`; expected
`PASS evac live=262144 iters=20000000 checksum=1021046735439613382 corrupt=0`).

## 7. Per-cycle allocation of the shards

Helpers build `EvacShard::default()` every cycle, so `forwards` and `work`
grow from empty (reallocation doublings on the hottest write of the pause).
**Proposal W5:** keep the shards in the pool between cycles (clear, keep
capacity), or pre-size `forwards` from the previous cycle's survivor count
divided by workers (the driver already keeps `prev_survivor_count`).

---

## 2026-09-24 round 4 wave 6 (lane `young6`): the pool's two serial chains, and W5

Re-read against `28f4acd3a`. §1-§5 and §7 are unchanged in `gen_evac.rs`
(another lane owns it this wave); §6 is as landed. This lane owns
`gc/src/evac_pool.rs` and the driver in `collect_garbage_inner`, and found two
scaling limits there that the page did not list, both fixed:

### A. The dispatch was a serial wake-up chain (new finding, FIXED)

`EvacPool::scope` notified each worker's condvar WHILE holding `state`.
`parking_lot`'s `Condvar::notify_one` does not wake a waiter whose mutex is
held: it REQUEUES it onto the mutex, and a mutex unlock wakes one waiter. So
the workers were released one at a time: worker k ran only after workers
0..k-1 had each been woken, taken `state`, read the job and dropped it — N
sequential futex round trips at the start of every parallel copy, growing
linearly with the worker count. The notify now runs after `state` is
released (the condvars are snapshotted under it; `dispatch` is held, so the
list cannot grow). Sound because the generation bump happens under `state`
and a worker's check-then-park is atomic under the same lock.

### B. The termination was an N-way convoy on `state` (§5, FIXED on the pool side)

Every finishing worker locked `state` to decrement `pending`, and the
evacuator's termination protocol ends a job with all workers idle at the same
instant. `pending` is now an `AtomicUsize` outside `state`
(`Inner::pending`): each worker does one `fetch_sub(AcqRel)`, and only the
one that reaches zero locks `state` to notify `done`. The driver waits under
`state` for an `Acquire` read of zero (no lost wake-up: the last worker's
notify needs the lock the driver's check-then-wait holds). A panicking worker
records its payload under `state` BEFORE its decrement, so the driver still
finds it. §5's other half (the `DrainState` mutex and its `notify_all` herd)
is `gen_evac.rs`'s and stays open (W1).

### C. W5, per-cycle shard allocation (FIXED in the driver)

`collect_garbage_inner` now reserves each shard's `forwards` from the previous
cycle's survivor count split evenly over the workers (the `map_hint` the
pointer map is already sized from). The shards themselves are still built per
cycle; keeping them in the pool between cycles is the rest of W5 and needs
`EvacShard` to be reusable (`gen_evac.rs`).

### Tests

`evac_pool::tests::back_to_back_dispatches_keep_the_barrier_with_the_atomic_pending_count`
(3000 dispatches at widths 1..=8: every worker's write visible after the
barrier, every worker run exactly the right number of times, no hang), plus
the existing pool tests (panic payload, driver panic, grow, narrow job,
drop), which exercise the changed barrier.

### Still open (cross-lane, `gen_evac.rs` / allocation)

W1 (per-worker deques, `notify_one`), W2 (parallel root / card seeding), W3
(evacuation slack reserved by the allocator), W4 (first promotion buffer
sized from the previous cycle), the rest of W5 (shards kept in the pool), and
an `EvacShard` age table for adaptive tenuring (so the driver stops reading
each to-space survivor's header when adaptive tenuring is on). Diffs are in
`docs/internal/reviews/gengc-round4-w6-young6-20260924.md`.
