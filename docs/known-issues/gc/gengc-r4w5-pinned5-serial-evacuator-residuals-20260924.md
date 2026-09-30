# The serial evacuator's three open residuals: the young→young map insert, the destination RMW, the scan prefetch

> **STATUS (2026-09-29, gce e2/y): OPEN -- item 1's measurement row written; item 3 retires with `gce-e1y-proposal-retire-the-never-evacuator-switches-DONE-20260929.md`.**
> - **Row:** `p ser_evac_$r 300 "CRATONVM_GC_PAR_EVAC=0 CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0" "-XX:+UseGenerationalGC -Xmx64m --nojit" "GenR4W4EvacThroughputProbe 65536 4000000"`.
> - **Pass:** stdout `PASS evac live=65536 iters=4000000 checksum=7690339906893138022 corrupt=0`.
> - **What to record** (it decides whether item 1 is worth a design): the `fwd_copies` and `cheney_drain` medians of the `[gcpause]` lines.

> **STATUS (2026-09-29, gce e1/x): KEEP -- nothing ran.** **Remaining:** item 1's `fwd_copies` vs `cheney_drain` measurement (`--nojit`); item 3 goes with the removal proposal (`gce-e1y-proposal-retire-the-never-evacuator-switches-DONE-20260929.md`).

> **STATUS (2026-09-29, gce e1/y): NARROWED -- item 1 stays a design item with no measured cost; item 3 should be REMOVED, not flipped. No code change.**
>
> - **Item 1 (the per-survivor `pointer_map` insert).** Re-read on `adb9178bc`: every consumer the page lists still reads "in the map" as "survived" (the VM's `update_all_roots`, `remap_external_roots`, `monitors.remap_after_gc`, reference processing, finalizer resurrection, the loader rescue; and on a pinned cycle the identity pairs are the only record of a pinned or kept survivor). Dropping it means a lazily materialised map (walk to-space's survivors and read each source address from a side word the copy leaves), which costs a to-space walk to save a hash insert. Not worth building before `fwd_copies` against `cheney_drain` is measured on a moving-heavy run (the command below, `--nojit`). The parallel arm's insert cost is `map_merge`, whose thread-spawn half is request 2 of this lane's report.
> - **Item 3 (the scan prefetch, `CRATONVM_GEN_EVAC_SCAN_PREFETCH`).** The d8 triage table marks it NEVER (no gain measured in two rounds, `w3_evac_w124p` inside noise). Recommendation to the orchestrator: remove the switch and `forward_ref_slots_pf` / `prefetch_card_root_referent` (the prefetching forwarder is `forward_ref_slots` itself with the switch off), which retires the item; the flag registry rows are the orchestrator's files. Kept unchanged here.
> - Verify (unchanged): `cargo test -j 5 -p cratonvm-gc --lib gen_evac` and the serial A/B command below (`PASS evac live=65536 iters=20000000 checksum=9065873663750453210 corrupt=0`).

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`): OPEN for items 1 and 3 only. Item 2 is DONE** (the destination's snapshot mark is stored once in `forward_object_impl` and `ParEvac::evacuate`, gcd d2/h; `r4w5_pinned_young_copy` and `gen_evac` tests pass in the round's Windows suite). Item 1 (the per-survivor `pointer_map` insert) is a design item with no measured cost; item 3 is an unmeasured opt-in. Nothing on these paths changed in waves d3-d7.

## STATUS (2026-09-27, gcd d2/h): re-verified on `a1fa77603`; item 2 LANDED (retire it); item 1 a design item with no measured cost; item 3 opt-in, unmeasured. Nothing changed on these three paths this wave.

- **Item 2 (the destination RMW):** in the code as described below
  (`forward_object_impl` and `ParEvac::evacuate` store
  `mark_with_gc_flags` / `mark_with_gc_age` of the snapshot once). Closed;
  the page stays only for items 1 and 3.
- **Item 1 (the per-survivor `pointer_map` insert):** unchanged, and the
  reasons it is load-bearing still hold on this base: `update_all_roots` /
  `remap_external_roots` / `monitors.remap_after_gc` resolve a young→young
  relocation only through the map, reference processing and finalizer
  resurrection read "in the map" as "survived", and on a pinned in-place cycle
  the identity entries are the only record of a pinned or kept survivor. The
  one sub-cost that was pure (a re-encounter probe of a to-space or in-place
  target) is already gone. What the insert COSTS is a work count, not a
  timing: `fwd_copies` in the `[gcpause]` line (`CRATONVM_DBG=gcpause`) is one
  insert per copy, and `fwd_reencounters` the probes that remain (old-gen
  targets only). On `GenR4W4EvacThroughputProbe`'s JIT-warm default arm those
  inserts are not the gap: most young cycles do not copy at all (see the
  evac7 page's gcd d2/h STATUS: term 3's helper-window verdict and term 4).
  Measure before designing: the command in "Verification" below with
  `CRATONVM_GC_PAR_EVAC=0 --nojit`, reading `fwd_copies` against
  `cheney_drain` per cycle.
- **Item 3 (the scan prefetch):** opt-in (`CRATONVM_GEN_EVAC_SCAN_PREFETCH`),
  unmeasured; unchanged. This wave's `forward_ref_slots_pf_at` (the slot
  offset variant the promoted scans now use, gcd d2/h) keeps the prefetch
  schedule byte for byte.

Verify (unit): `cargo test -j 5 -p cratonvm-gc --lib r4w5_pinned_young_copy`
and `cargo test -j 5 -p cratonvm-gc --lib gen_evac` pass. Runtime: the
serial A/B command below prints
`PASS evac live=65536 iters=20000000 checksum=9065873663750453210 corrupt=0`.

## Earlier status (2026-09-27, gen r5w6/pin10; superseded by the block above): item 2 LANDED on by default (bit-identical); item 1 narrowed; item 3 as before

1. **The map insert: narrowed, still a design item.** One sub-cost is gone.
   - On a pinned in-place cycle, a RE-ENCOUNTER of a survivor already
     forwarded into a from-space destination span no longer probes
     `pointer_map` (`forward_object_impl`'s already-forwarded arm). That
     forward was installed by this cycle, and its pair is already in the map:
     the serial arm inserts it right after the install; the parallel arm
     inserts it in `map_merge`, which comes before every serial forward.
   - This is the rule `gen_evac::ParEvac::evacuate` already applied to its
     `in_place_target`, and the Cheney path applies it to to-space targets.
   - The per-copy insert itself stays, for the reasons below.
2. **The destination RMW: LANDED, on by default.** It is a pure cost
   reduction with a bit-identical result.
   - New pure helpers in `types/src/heap_types.rs`:
     `ObjectHeader::mark_with_gc_age(mark, age)` and
     `ObjectHeader::mark_with_gc_flags(mark, flags)`. The second refuses
     `GC_FLAG_COMPACT` in debug builds, because `add_gc_flags` also clears the
     second word for it.
   - `forward_object_impl` and `ParEvac::evacuate` compute the destination's
     mark from the snapshot, then store it once: the old-gen flag for a
     promotion, age + 1 otherwise. This replaces a store plus a locked
     `fetch_or` or a compare-exchange loop per survivor.
   - The age census reads the age from the computed word.
3. **The scan prefetch:** unchanged (opt-in, `CRATONVM_GEN_EVAC_SCAN_PREFETCH`).

**Verify (unit):**

```bash
cargo test -j 5 -p cratonvm-types --lib the_pure_mark_helpers_agree_with_the_setters
cargo test -j 5 -p cratonvm-gc --lib r4w5_pinned_young_copy
cargo test -j 5 -p cratonvm-gc --lib gen_evac
cargo test -j 5 -p cratonvm-gc --lib
```

All must pass. `a_copy_carries_the_source_mark_with_its_age_bumped_and_reencounters_converge`
pins both items: the copy's mark equals `mark_with_gc_age(source, 1)`, and two
roots plus a slot converge on one copy.

**Verify (A/B, ABBA, one host).** Base `fbcb272ab` against this lane's
commit. The command below must print
`PASS evac live=65536 iters=20000000 checksum=9065873663750453210 corrupt=0`
on every run:

```
CRATONVM_GC_PAR_EVAC=0 CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 \
  cratonvm --java-home "$JDK" --nojit -XX:+UseGenerationalGC -Xmx64m -cp tools/bench \
  GenR4W4EvacThroughputProbe 65536 20000000
```

Compare the `cheney_drain` medians. Expected: a few percent lower, about one
locked instruction per survivor. It will not be visible in `time_ms` through
the ~3x noise.

## Earlier status (2026-09-26, gen r5w3/evac7; superseded by the block above): item 3 LANDED behind `CRATONVM_GEN_EVAC_SCAN_PREFETCH`; items 1 and 2 still open

1. **The map insert: open, design.** The reasoning is unchanged. A pinned
   in-place cycle's identity entries are load-bearing. On this lane's reading
   the insert is not among the top costs of a young cycle on
   `GenR4W4EvacThroughputProbe`; see the breakdown in the gen r5w3/evac7
   STATUS of
   `gengc-r4w4-young4-parallel-evacuator-scaling-limits-20260924.md`.
2. **The destination RMW: open, a cross-lane request to the `types` owner**,
   unchanged. It needs pure `ObjectHeader::mark_with_gc_age(mark, age)` and
   `mark_with_gc_flags(mark, flags)` in `types/src/heap_types.rs` (no lane
   owns that file this wave). It is one uncontended locked RMW per survivor
   on a line the copy just wrote: small next to the costs named there.
3. **The scan prefetch: LANDED, opt-in.** `gen_heap::forward_ref_slots_pf`
   is used by the serial Cheney scan, the promoted-object scan, the in-place
   scan (`scan_in_place_young`, the LIFO over scattered survivors wave 6
   singled out) and the parallel `scan_object`. It is `forward_ref_slots`
   itself with the flag off. It prefetches a reference array's referents
   8 elements ahead and an instance's referents all first.
   `seed_dirty_card_roots` prefetches the card root 8 entries ahead.

**Verification.** The command below must print
`PASS evac live=65536 iters=20000000 checksum=9065873663750453210 corrupt=0`
with and without `CRATONVM_GEN_EVAC_SCAN_PREFETCH=1`. Then compare the
`cheney_drain` / `card_root_forward` medians ABBA.

```
CRATONVM_GC_PAR_EVAC=0 CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 \
  cratonvm --java-home "$JDK" --nojit -XX:+UseGenerationalGC -Xmx64m -cp tools/bench \
  GenR4W4EvacThroughputProbe 65536 20000000
```

`--nojit` is there because a JIT-warm run of this probe never copies (term 4
diverts every cycle). The same pair on the pinned arm is
`GenR4W6PinnedDefaultGauntletProbe` with `CRATONVM_GEN_PINNED_YOUNG_COPY=1`,
expecting
`PASS gauntlet threads=4 calls=100000 depth=24 bad=0 checksum=45190562680832`.

## Earlier status (2026-09-26, gen r5w1/young5; superseded by the block above): still OPEN; nothing landed, by rule

Re-read against `9e252c8b2`; all three items are as described.

1. **The map insert** stays a design item (unchanged reasoning: every
   consumer of `pointer_map`, and on a pinned cycle the identity entries, read
   "in the map" as "survived").
2. **The destination RMW** still needs pure mark-word helpers in
   `types/src/heap_types.rs` (not this lane's file). Exact request, unchanged:
   `ObjectHeader::mark_with_gc_age(mark, age) -> mark` and
   `mark_with_gc_flags(mark, flags) -> mark`; then in `forward_object_impl`
   store `mark_with_gc_age(mark_snapshot, age + 1)` (young copy) or
   `mark_with_gc_flags(mark_snapshot, GC_FLAG_OLD_GEN)` (promotion) as the
   destination's single mark store, replacing the store + `set_gc_age` /
   `add_gc_flags` pair; the same two lines in `gen_evac::ParEvac::evacuate`.
3. **The scan prefetch** is a behaviour-neutral but timing-changing edit of
   the copy's hottest loop, so it must be opt-in, and a new opt-in flag means
   four registry files (`types/src/flag_groups.rs`, `types/tests/flag-surface.txt`,
   `docs/flag-tokens.md`, `docs/config/flag-inventory.md`, the last two
   GENERATED by `tools/flag-census/*` whose counts a test enforces). This lane
   could not run the generators (no Python on its host), and a hand-edited
   generated row with a wrong count turns `cargo test -p cratonvm-types` red.
   The code and the registry diff are in
   `docs/internal/gc/gengc-r5w1-young5-proposal-evacuator-contained-wins-DONE-20260928.md`.

Probe: unchanged (the verification section below). The pinned arm's
`cheney_drain` should be read separately, as wave 6 asked.

---

Slug: `gengc-r4w5-pinned5-serial-evacuator-residuals`
Filed 2026-09-24 by generational GC round 4, wave 5, lane `pinned5`.
**Earlier status (superseded by the block above): OPEN** (split out of the retired
`docs/internal/gc/gengc-r4-move-serial-evacuator-hot-path-FIXED-20260924.md`,
whose items 1-3 and header decode are in the code).
**Severity: perf** (`cheney_drain` on every serial moving cycle — small heaps,
`CRATONVM_GC_PAR_EVAC=0`, and since wave 5 every pinned in-place cycle, which
is serial by construction).

## Location

`gc/src/gen_heap.rs`, `GenerationalHeap::forward_object_impl` (the serial
per-object copy) and the serial drain loops in `collect_garbage_inner`,
`redrain_serial_closure` and Phase 2.5b; `types/src/heap_types.rs`
(`ObjectHeader` mark-word layout, not this collector's file).

## Evidence (by reading, at `5744a3944` + wave 5)

1. **Every young→young copy inserts into `pointer_map`** (an `FxHashMap`):
   `pointer_map.insert(old_ptr, new_ptr)` right after
   `set_forwarding_address`. The Cheney scan never reads the map — a
   re-encounter reads the forwarding word in the source header — so the insert
   exists only for the VM's post-GC root remap, `remap_external_roots`,
   `monitors.remap_after_gc`, reference processing (`is_in_young_either` + "not
   in the map ⇒ dead"), finalizer resurrection and the loader rescue. One hash
   insert per survivor, and `prev_survivor_count` pre-sizing exists only
   because of it.
2. **The destination header takes a read-modify-write.** `set_gc_age` is a
   `compare_exchange_weak` loop (young copy) and `add_gc_flags` a `fetch_or`
   (promotion), on a header no other thread can see yet. The age and flag bits
   could be folded into `mark_snapshot` before the single destination store,
   but the layout (`AGE_SHIFT`, `FLAGS_SHIFT`) is private to
   `types/src/heap_types.rs`.
3. **The scan does not prefetch.** The Cheney scan is breadth-first over
   to-space (and, on a pinned in-place cycle, a LIFO over the in-place
   worklist), so its from-space referent header reads are random. HotSpot hides
   them with `PrefetchScanIntervalInBytes` / `PrefetchCopyIntervalInBytes`.

## Failure scenario

No correctness failure. Each is per-object work on the copy phase's hottest
path: (1) a random-access hash insert per survivor, (2) an atomic RMW per
survivor, (3) a cache miss per referent that a two-pass scan could overlap.

## Fix and sizing

1. **Large, design.** Prove every consumer listed above can resolve a
   young→young relocation through the forwarding header instead (the header is
   only valid until from-space is reset or rebuilt, so the VM's remap would
   have to run before that point, or the map be materialised lazily from a
   to-space walk). Not before a measurement says the insert matters.
2. **Small, cross-lane.** Request to the `types` owner: pure
   `ObjectHeader::mark_with_gc_age(mark: u64, age: u8) -> u64` and
   `mark_with_gc_flags(mark: u64, flags: u8) -> u64`. Then a two-line change in
   `forward_object_impl`: compute the destination mark from `mark_snapshot`
   and store it once.
3. **Small-medium, behind an A/B flag.** A two-pass `forward_ref_slots` for
   the serial scan: prefetch every from-space referent header of the object,
   then forward.

The wave-4 A/B for the retired items was never taken; take it first, because
it prices all three:

## Verification

```
CRATONVM_GC_PAR_EVAC=0 CRATONVM_DBG=gcpause,gc-stats CRATONVM_DBG_GCPAUSE_MIN_US=0 \
  cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -cp tools/bench \
  GenR4W4EvacThroughputProbe 65536 20000000
```

must print `PASS evac live=65536 iters=20000000 checksum=9065873663750453210 corrupt=0`;
compare `cheney_drain` medians and `fwd_copies`, ABBA interleaved on one host
(in-JVM timings swing ~3x between repetitions on the shared box). For item 2
or 3, the same command, base vs change.

---

## 2026-09-24 round 4 wave 6 (lane `pinstale6`): still OPEN, nothing landed

All three items are unchanged at `28f4acd3a`. None of them was safe to land
unmeasured this wave:

1. **The map insert** is a design item. On a pinned cycle it is
   load-bearing twice over. The identity entries `X→X` are how reference
   processing, finalizer resurrection and the loader rescue learn that a
   pinned or kept object survived. So an in-place cycle cannot drop the insert
   even where a Cheney cycle could.
2. **The destination RMW** still waits on the `types` owner's pure mark-word
   helpers. The cross-lane request stands as written above.
3. **The scan prefetch** is a perf change on the copy's hottest loop. It
   needs an A/B flag and a measured win, and this wave could neither build
   nor run.

One new observation for item 3. On a pinned cycle the in-place worklist is
LIFO (`InPlaceEvac::scan` is a `Vec` popped from the back), and its
destinations are scattered free spans. A prefetch there has to cover both the
referent headers and the destination lines. The Cheney scan's destination is
the contiguous to-space tail, which the hardware prefetcher already follows.
Measure the pinned arm separately:
`GenR4W6PinnedDefaultGauntletProbe` with `CRATONVM_GEN_PINNED_YOUNG_COPY=1`
and `CRATONVM_DBG=gcpause`, reading `cheney_drain` of the
`moving-pinned-pages` cycles.
