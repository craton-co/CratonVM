# The pinned in-place young copy: five limits of the first cut

> **STATUS (2026-09-29, gce e1/x): KEEP -- perf, flag-on only; nothing ran.** **Remaining:** L3 (an `Arena::grow_in_place` over a reserved store) and L5 (the d5/s proposal).

> **STATUS (2026-09-29, gce e1/y): OPEN (perf, flag-on only) -- L3 and L5 NARROWED to exact designs; no code change (neither is contained in this lane's files, and neither is on the default path).**
>
> - **L3 (no nursery growth on a pinned cycle), narrowed.** Phase 6's growth needs an EMPTY arena because `Arena::grow` asserts `cursor == 0` (its backing may move). But the young stores are `HeapStore::Reserved`, and `HeapStore::grow_to` (`gc/src/reservation.rs`) returns the SAME base whenever `new_capacity <= reserved_len`: from-space can grow in place with live objects in it. What is missing, all in `gc/src/arena.rs` (lane o): an `Arena::grow_in_place(new_capacity) -> bool` that succeeds only on that arm (base unchanged), moves `high_cursor` / `pristine_hi` to the new end (the young arenas never serve the high end: `debug_assert_young_arena_is_low_only`), and extends the object-start bitmap and the commit bitmap (both are sized from the capacity at construction). The gen side (this lane) is then the pinned branch of Phase 6: when the idle semi-space is grown, grow from-space in place too, and republish `region_bounds` / `JIT_REGION_BOUNDS` (the base does not move, only the end; `assert_region_encodable` must still pass). Refused growth leaves today's behaviour. Probe: `CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W6PinnedDefaultGauntletProbe` prints `PASS gauntlet threads=4 calls=100000 depth=24 bad=0 checksum=45190562680832`, and the `[GC] generational:` young capacity grows past its start value (today it never does on a run whose every cycle is pinned).
> - **L5 (every object on a pinned page is a root), narrowed.** The proposal `gcd-d5s-proposal-pinned-copy-roots-by-word-not-by-page-20260928.md` is the design; this lane re-read `build_pinned_young_plan` and confirms the page grain is only needed for the PLACEMENT (no destination on a pinned page), not for the ROOT set: a page's objects other than the ones a ledger word resolves to could be ordinary (copyable, collectable) survivors that simply stay in place (`kept`, not roots). Its cost is one cycle of floating garbage per pinned page, and it feeds the retention shape of row 3 of the flip gate only through requested majors, which the WIDE true-root seed (gcd d9/a) now handles. Keep it as the proposal's.
> - Verify (unchanged): `cargo test -j 5 -p cratonvm-gc --lib r4w5_pinned_young_copy`; the two probes above with their PASS lines.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): unchanged -- OPEN (perf, flag-on only), L3 and L5 open.** The flag-on retention this page's L2/L5 were feeding (`../../internal/gc/gcd-d5s-pinned-copy-keeps-a-dropped-chain-across-requested-majors-FIXED-20260928.md`) is no longer reproduced on d7 and retires this wave (arm D 5/5, D+hold 5/5 on `GenR4W6JitOomRootProbe`); the flag-on timeouts left (`armC_5`, `armDhk0_5`) end in thousands of non-moving fallback sweeps, the livelock that also hits a default arm (`../../internal/gc/gcd-d8x-heap-full-oome-shapes-livelock-on-fallback-young-cycles-FIXED-20260928.md`). The pinned copy's own probes pass (`pinned_young_copy`, `pinned_gauntlet`).

## STATUS (2026-09-28, gcd d5/s): OPEN (perf, flag-on only); no code change; L2 and L5 now also feed a flag-on RETENTION defect

Re-read on `d916d1c40`; L1-L5 are as the block below says. What matters
now, in order:

- **Nothing here is on the default path.** Every limit belongs to the
  pinned in-place cycle, which runs only with `CRATONVM_GEN_PINNED_YOUNG_COPY`
  (or, on the default path, on the rare cycle whose card walk-gap words build
  a plan: `card_gap_roots.young_words` in `collect_garbage_inner_with_pins`;
  L1-L5 apply there too, but that cycle is a repair, not a throughput path).
- **L2 and L5 are also correctness inputs now, not only perf.** A pinned
  object stays young while its neighbours are promoted (by age, or at age 1
  when the destination spans run out: L2's `pycopy_overflow_promotions`),
  and every object on a pinned page is a root for the cycle (L5). Together
  they build old -> young edges inside dead data, which a requested major
  then keeps alive: `../../internal/gc/gcd-d5s-pinned-copy-keeps-a-dropped-chain-across-requested-majors-FIXED-20260928.md`
  (a flip blocker, row 3a of the flip gate). L5's narrowing is filed as
  `gcd-d5s-proposal-pinned-copy-roots-by-word-not-by-page-20260928.md`.
- **L4 (in-pause zeroing) and L3 (no nursery growth on a pinned cycle)**
  are the two left that only cost time. L4's remaining half (hand the dead
  spans to the wipe thread) needs the arena to accept a free block that is
  not yet zero, which the refill tripwire forbids; L3 needs a swap that a
  pinned cycle never makes. Neither is contained; neither was changed.

Verification commands are unchanged (the block below): the two unit-test
modules and `GenR4W5PinnedYoungCopyProbe` / `GenR4W6PinnedDefaultGauntletProbe`
with the flags on, which must print their PASS lines.

## Previous STATUS (2026-09-26, gen r5w3/evac7): L1 and L2 LANDED behind opt-in flags; L4's zeroing is now actually parallel; L3 and L5 open

- **L1, the serial copy: LANDED behind
  `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL`** (needs
  `CRATONVM_GEN_PINNED_YOUNG_COPY`; default off). Wave 6's four changes are
  built in `gc/src/gen_evac.rs` (`ParInPlace`, plus in-place arms in
  `ParEvac::evacuate`):
  1. **A span pool.** The serial plan's destination spans, each with a
     CAS-advanced cursor and a shared "first useful span" index. A worker
     claims a 4 KiB buffer, or an exact span for an object of 1 KiB and up.
     It searches 32 spans past the first live one, plus the tail window.
  2. **Pinned objects are claimed by a SELF-FORWARD**, not by a side
     `AtomicBool`. The claimer records `(X, X)`, queues `X` and saves the
     mark word. A peer reads the forward and gets `X` back. The driver undoes
     every claim (`restore_self_forwarded_mark`) right after the drain,
     before any header is read again, and marks the objects `visited` in the
     serial state (`absorb_parallel_in_place`).
  3. **"Kept" survivors are claimed the same way.** The self-forward, not a
     bitmap, is what stops a second worker whose own buffer still fits the
     object from copying it too.
  4. **`dest_written` over the pool.** A slot re-read after its rewrite is
     not recorded as a refusal.

  The rest of the design:
  - A forwarding-CAS loser ZEROES its abandoned young copy instead of
    stamping a filler: the rebuild frees every byte no survivor occupies
    without wiping it.
  - No buffer tail gets a filler; unused span bytes were zero and stay zero.
  - After the drain, the in-place results (`live`, `kept`, the counters, and
    every span's cursor) are folded back into the serial `InPlaceEvac`. So
    the loader rescue, finalizer resurrection, the census and
    `finish_in_place_young_cycle` run unchanged.
  - The dirty-card roots can be seeded on the workers too
    (`CRATONVM_GC_PAR_EVAC_CARD_SEED`).
- **L2, the tail window: LANDED behind
  `CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE`.** The window is
  `max(used - free_list_bytes, 1 MiB)` instead of `max(used / 2, 1 MiB)`,
  still capped by `capacity - used`: wave 6's design. The arming is factored
  out as `GenerationalHeap::arm_in_place_young_cycle`, which both arms call.
  With the flag off it computes exactly what the serial arm did.
- **L4: part landed, on by default (the set of bytes zeroed is unchanged).**
  `young_mark::zero_spans_parallel` ran ONE thread per span and fell back to
  one thread for fewer spans than workers. The pinned rebuild's dead set is
  exactly that shape: a few huge spans between a handful of pinned objects.
  So its "parallel" zeroing was serial. It now cuts spans into per-worker
  pieces and falls back to serial by total bytes (under 1 MiB). The
  non-moving sweep's many-span zeroing is unaffected in effect. Deferring the
  zeroing to the wipe thread (the rest of L4) is still open.
- **L3, L5:** unchanged.

**Tests** (`cargo test -p cratonvm-gc --lib r4w5_pinned_young_copy` and
`young_mark::tests`):

- `a_parallel_pinned_cycle_keeps_the_pinned_object_and_copies_the_rest`: the
  parallel arm engaged, the pinned object did not move, its mark word is not
  forwarded afterwards, and the in-place copy census moved.
- `a_parallel_pinned_cycle_keeps_a_wide_graph_intact_across_two_cycles`: 2000
  referents of a reference array plus a pinned page, W2 and the prefetch on;
  every value survives this cycle and the next one, which must walk the
  rebuilt from-space.
- `zero_spans_parallel_splits_a_few_huge_spans_exactly`.

**Probes** (flags on; ABBA against the serial pinned arm; checksums unchanged,
and also under `CRATONVM_DBG=gc-stress=250000`):

```
CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL=1 CRATONVM_DBG=gc-stats,gcpause \
  cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W5PinnedYoungCopyProbe
```

must print `PASS pinned threads=4 calls=600000 bad=0 checksum=-6144781693192940271`.
`GenR4W6PinnedDefaultGauntletProbe` must print
`PASS gauntlet threads=4 calls=100000 depth=24 bad=0 checksum=45190562680832`.
Read the `evac_drain` medians of the `moving-pinned-pages` cycles against the
serial arm's `cheney_drain`, and `[GC] young_pinned_copy:`
`pycopy_overflow_promotions` with and without
`CRATONVM_GEN_PINNED_TAIL_WINDOW_LIVE=1`.

**Caveat.** The pinned copy itself is blocked from a default flip by the
netty SIGSEGVs. See the gen r5w3/evac7 STATUS of
`gengc-r4w6-pinstale6-pinned-copy-default-flip-gate-20260924.md`. The
parallel arm inherits that block.

## Earlier status (2026-09-26, gen r5w1/young5; superseded by the block above): still OPEN (perf follow-ups); re-read, probe commands correct

Re-read against `9e252c8b2`: L1-L5 are as described (the serial-only forcing
`par_workers = 1` on `in_place_cycle`, the up-front tail window, Phase 6
skipped on a pinned cycle, the in-pause `zero_spans_parallel`, the pinned-page
roots). The verification command below is right. For the default flip
(pinstale6's gate), L1 and L4 are the two that can make a pinned cycle's pause
LONGER than the non-moving sweep it replaces on a large young generation, and
L2 the one that can raise the major count; the flip-gate page now asks the
gauntlet to read exactly those.

---

Slug: `gengc-r4w5-pinned5-in-place-copy-limits`
Filed 2026-09-24 by generational GC round 4, wave 5, lane `pinned5`.
**Earlier status (superseded by the block above): OPEN** (follow-ups; none is a correctness defect). Wave 6
(`pinstale6`) designed L1 and costed L2; see the section at the end.
**Severity: perf** — only with `CRATONVM_GEN_PINNED_YOUNG_COPY` on.

## Location

`gc/src/gen_heap.rs`: the `InPlaceEvac` block near the top of the file
(`pinned_young_pages`, `build_pinned_young_plan`, `InPlaceEvac::dest_alloc`),
the pinned branch after `divert_non_moving` in `collect_garbage_inner`, the
serial arm's in-place setup, `forward_object_impl`'s in-place arms,
`finish_in_place_young_cycle`; `gc/src/arena.rs`,
`Arena::rebuild_after_in_place_evacuation`. Design and hazard walk:
`docs/internal/reviews/gengc-round4-w5-pinned5-20260924.md`.

## Evidence and failure scenario, per limit

**L1 — serial only.** `par_workers` is forced to 1 on a pinned cycle. The
destinations are from-space's free spans, scattered between live objects, not
the contiguous to-space tail `ParEvac::plan` reserves and carves into
per-worker PLABs; and the pinned identity forward would need a CAS-claimed side
bit per worker. Scenario: a JIT-warm heap past `CRATONVM_GC_PAR_MIN_BYTES`
(16 MiB of young) loses the parallel copy's speed-up on every pinned cycle.

**L2 — bounded destination capacity.** A young survivor goes to a from-space
span that was FREE at the start of the cycle: the free blocks, plus a tail
window of `min(capacity - used, max(used / 2, 1 MiB))` bytes, committed up
front. Beyond that it is promoted early (`pycopy_overflow_promotions`), and
beyond old gen it stays where it is (`pycopy_kept`). Scenario: a high-survival
JIT-warm phase (every cycle pinned) tenures survivors at age 1 instead of
`PROMOTION_AGE` 3, filling old gen with medium-lived data.

**L3 — no nursery expansion.** Phase 6's young growth runs only on a Cheney
cycle (`!in_place_cycle`): growing the idle semi-space pays only after a swap,
and a pinned cycle does not swap. Scenario: a workload whose every cycle is
pinned never grows young past its current semi-space.

**L4 — dead bytes zeroed inside the pause.** From-space stays the allocation
space, so its dead bytes (the old copies of evacuated objects, dead objects)
are zeroed before the pause ends (`young_mark::zero_spans_parallel`, on the
young GC workers), as the non-moving sweep zeroes its dead spans. The Cheney
path instead hands the evacuated semi-space to the off-pause wipe. Scenario: a
pinned cycle's pause carries an `O(allocated)` memset the Cheney pause does
not.

**L5 — one cycle of floating garbage per pinned page.** Every object that
overlaps a pinned page is a ROOT of the cycle (kept alive, not only kept in
place — a derived word may be the only reference a compiled frame holds), so a
dead neighbour on the page, and everything it reaches, survives one cycle; its
`finalize()` and any `Reference` to it wait one cycle. Bounded by the pinned
pages (≤ 1/8 of from-space by construction; the census that sized the design
had 2.1 young pins per cycle on average).

## Fix and sizing

* L1 (medium): a per-worker destination span list carved from the free spans
  at plan time (each worker bump-allocates in its own spans, no sharing), and
  the pinned side bit as an `AtomicBool` per pinned object claimed by CAS.
* L2 (small): size the tail window from the previous cycle's young survivor
  bytes (`prev_survivor_count` has the object count already), or let the
  window grow by re-committing between drain rounds.
* L3 (small-medium): grow from-space in place when its reservation allows it
  (`Arena::grow` asserts an empty arena today because `Vec::resize` may move;
  a reserved `HeapStore` does not move).
* L4 (medium): defer the zeroing of dead spans ABOVE the new cursor to the
  wipe thread and keep the cursor below them until the wipe is joined — the
  allocator only needs the bump tail zero at hand-out time.
* L5: none needed unless the census shows `pycopy_bytes_sum` large.

## Verification

With the flag on:

```
CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DBG=gc-stats,gcpause \
  cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench \
  GenR4W5PinnedYoungCopyProbe
```

must print `PASS pinned threads=4 calls=600000 bad=0 checksum=-6144781693192940271`.
Read `[GC] young_pinned_copy:` — `pycopy_overflow_promotions` (L2),
`pycopy_kept` (L2), `pycopy_bytes_sum / pycopy_cycles` (L5) — and the
`[gcpause]` rows of the pinned cycles (L1, L4) against a `--nojit` control.

---

## 2026-09-24 round 4 wave 6 (lane `pinstale6`): still OPEN; L1 designed, L2 costed

No limit is closed by this wave. None of the five is a correctness defect.
Each fix is a new code path on the copy phase's hottest code, and this wave
could not compile or measure. The wave-6 review is
`docs/internal/reviews/gengc-round4-w6-pinstale6-20260924.md`.

### L1: parallel evacuation for pinned cycles (design, not landed)

The parallel evacuator (`gc/src/gen_evac.rs`, `ParEvac`) assumes ONE
contiguous destination region, `[EvacPlan::region_start, region_end)`. Workers
carve PLABs from it with a shared bump cursor. A pinned cycle's destinations
are many disjoint free spans of from-space. Four changes are needed, all in
`gen_evac.rs` plus the driver in `collect_garbage_inner`:

1. **A span pool in place of the region.** Add
   `EvacPlan::spans: Option<Arc<[(AtomicUsize /*cursor*/, usize /*end*/)]>>`
   and an `AtomicUsize` index of the first span that may still have room.
   `ParEvac` carves a PLAB by CAS on the current span's cursor. When a span
   has less than a minimum PLAB left, the worker moves the index forward, so
   there is no per-object search. A too-large object (bigger than a PLAB)
   takes an exact span with the same CAS. The tail window stays the last
   span. A PLAB tail the worker retires gets `write_filler`, exactly as a
   to-space PLAB tail does today. It is `live`-adjacent, and the rebuild frees
   it with the rest of the complement.
2. **Identity forwards for pinned objects by CAS.** Replace
   `InPlaceEvac::visited: Vec<bool>` with `Vec<AtomicBool>`. The worker whose
   `compare_exchange(false, true)` succeeds pushes the object on its own scan
   stack and records `(X, X)` in its shard's `forwards`. Every other worker
   returns `X`. `pinned_index` / `covers_pinned` are read-only after the plan.
3. **"Kept" needs a claim that does not touch the header.** The serial path
   keeps a survivor in place when neither a span nor old gen can take it. It
   records the survivor in `kept` and installs no forwarding word, because
   the object stays live and its mark word must survive. In parallel, two
   workers can reach the same survivor, so one of them must claim it. Add a
   from-space claim bitmap with the `ObjectStartBits` layout (one bit per 8
   bytes, `fetch_or`). The winner scans the object and pushes `(X, X)` and
   `(X, size)` into its shard. A losing worker returns `X`, and so does any
   later worker that finds the bit set before it tries a copy (test the bit
   before the forwarding-word CAS).
4. **`dest_written` over the pool.** A card slot re-read after it was
   rewritten names a destination byte, not an object start. The serial path
   answers this with `dest_written`. The parallel path has to answer "is
   `addr` below some span's cursor?", reading each cursor with Acquire. This
   is safe: a slot is rewritten only after the copy it names has completed,
   and that copy advanced its span's cursor before the forward returned.
   Without this test the parallel arm would record every such re-read as a
   `not-an-object-start` refusal.

The driver merges per-worker `live` and `forwards` lists as it does today
(`par_extend_pairs`). `finish_in_place_young_cycle` is unchanged.

Sizing: about 350–450 lines in `gen_evac.rs`, about 60 in the driver, and a
new `par_evac_census` field for pinned cycles.

Gate: the same flag, `CRATONVM_GEN_PINNED_YOUNG_COPY`, plus the existing
`CRATONVM_GC_PAR_EVAC`. Evidence to flip it: `GenR4W5PinnedYoungCopyProbe`
and `GenR4W6PinnedDefaultGauntletProbe` with `CRATONVM_DBG=gcpause`. Compare
the `cheney_drain` medians of the `moving-pinned-pages` cycles, serial against
parallel, ABBA interleaved on one host. The checksums must be unchanged, and
must also hold under `gc-stress=250000`.

Unit tests to write with it:
- a pinned object reached by two workers is identity-forwarded exactly once;
- a kept survivor reached by two workers is scanned exactly once;
- a PLAB never straddles a pinned page;
- a slot re-read after its rewrite is not a refusal.

### L2: what a larger tail window would cost

The obvious L2 fix is a window of `used - free_list_bytes`. It provably holds
every survivor: the tail alone is then as large as everything allocated.
Its costs, read from the code:

- **Commit charge, not memory.** `Arena::commit_for_relocation` commits the
  window up front. Untouched committed pages cost commit charge (on Windows)
  and no physical memory. The next Cheney cycle's `uncommit_evacuated_young`
  gives them back.
- **No extra zeroing on the default path.** `commit_for_relocation` also
  calls `note_writable`, which removes the whole window from the arena's
  monotone pristine window (`Arena::pristine_lo`). That matters only under
  the opt-in `CRATONVM_GEN_PRISTINE_CHUNKS`. The default zero-once skip
  (`GcFlags::gen_zero_once`) already leaves a bump-tail hand-out
  un-memset, and after `finish_in_place_young_cycle` every byte above the new
  cursor reads zero.

Not landed: it changes how much a pinned cycle commits, and nothing measured
it. The probe that would justify it is `GenR4W6PinnedDefaultGauntletProbe`
with `CRATONVM_DBG=gc-stats`. Take the `pycopy_overflow_promotions` /
`pycopy_cycles` ratio before and after; the checksum must be unchanged.
