# A young root the moving collector refuses to forward is left pointing into a zeroed from-space

> **STATUS (2026-09-29, gce e1/x): KEEP -- no change landed; the check rows were not re-read.** **Remaining:** the producer hunt (1), residual (a), and residual (b)'s exclusion-set guard in `collect_garbage_inner_with_pins` (e1/o's diff for the young lane); re-check no `[unrecorded-young-root]` line on the evac rows' stderr.

> **STATUS (2026-09-29, gce e1/o): NARROWED by reading; no code change (the refusal guard and the moving decision are the young lane's functions in `gen_heap.rs`).**
> - **(1) Producer hunt, allocation side: none found.** Every door that hands a young span to a mutator was re-read against "can a live object sit where the object-start walk does not look": `GenerationalHeap::refill_tlab_at_least` (records the chunk start with `note_object_start` under the lock; the owner bumps contiguously from it; the gcd d1/d floor declines instead of carving a buffer the object cannot use), `try_alloc_young_initialized` (records every slow-path object; the large-object split writes the header under the lock), `Tlab::keep_on_miss` (the missed object goes through the recorded slow path), `Tlab::retire` (the attached sink takes a tail only when its first AND last words read zero, whole-tail under `CRATONVM_DBG_DEADREF_STORE`; else the filler, whose O(1) tripwire counts `filler_over_object`), `GenerationalHeap::return_tlab_tail` (only the current from-space). None hides an object from the walk by reading, and the d7 verification's tripwires were zero. The remaining producer candidates are outside allocation: a stale root (a root that was already dangling), or a writer below the cursor.
> - **(b) narrowed: where a moving cycle has conservative roots on the default path.** The guard in `collect_garbage_inner_with_pins` is skipped whenever `has_conservative_roots`; a moving cycle with a live compiled frame happens by default only when term 4 is cleared by the pause's pin ledger (`term4_ledger_cleared`, `gc_quiescence::young_pin_ledger_clears_term4`, gcd d1/e option B), with the opt-in pinned copy (`pinned_young_words`), or under `CRATONVM_GC_PRECISE_ONLY_ROOTS=1` (no conservative population at all -- there the guard could simply run). On the ledger-cleared cycle the conservative population is KNOWN: the ledger's words plus the blocked peers' interior native-stack words (`plan_takes_blocked_peer_words`). **Proposed diff (young lane):** give `unrecorded_young_root_refuses_cycle` an exclusion set -- those words, and `pinned_young_words` when present -- and drop the `!has_conservative_roots` / `pinned_young_words.is_none()` conjuncts on those cycles; a from-space root outside the set is then precise and refuses the cycle exactly as today's precise-root arm does (same one-refusal latch). Unit test shape: a ledger-cleared cycle with one conservative interior word (not refused) and one planted unrecorded precise root (refused once).
> - **(a) unchanged:** card slot values and overlay roots are read after the card take; refusing there loses card edges. It needs a read-only peek of the dirty cards' slot values before the take (O(dirty cards) per moving cycle), in the same function.

> **STATUS (2026-09-28, gcd d8/x, final verification on the round branch at `307f0c6a2`, wave d7, Linux release): the landed fix's check PASSES; OPEN for the producer hunt (1) and the fail-open residuals (a)(b).** No `[unrecorded-young-root]` line in any of the 11 rows: `GenR4W4EvacThroughputProbe 65536 20000000 --nojit -Xmx64m` prints `PASS evac live=65536 iters=20000000 checksum=9065873663750453210 corrupt=0` (`s10_evac_nojit`), and `CRATONVM_TLAB_SHARE_SIZER=1 GenR5W3ConcUnloadProbe` 10/10 without the warning (`s10_share_unload_1..10`; half of them print `dead-loader-unloaded=false`, which is the class-unload page's, not this one's). Remaining unchanged: name a producer of an unrecorded young root, and cover card/overlay values and cycles with live compiled frames.

> **STATUS (2026-09-27, gcd d2/h): FIX (2) LANDED for the PRECISE root set,
> default ON (a dangling-root fix); NARROWED to the producer hunt (1) and the
> two root sources the fix does not cover. Not (A)'s mechanism.**
>
> *(A)* is fixed by gcd d1/c (`mark_young_to_old_refs` seeds the loaders of
> parseable young objects), and gcd d1/d found nothing size-dependent in the
> refill / sink / card paths (d1d's refill floor also stops a refill from
> carving less than the object). Nothing on this base names a producer that
> hides a live young object from the object-start walk; the exit was still
> fail-open by reading.
>
> **Landed (`gc/src/gen_heap.rs`):**
>
> - `collect_garbage_inner_with_pins`, right after the object-start walk and
>   BEFORE the dirty-card take and any copy: on a cycle whose root set is
>   precise (`compiled_frames_live()` false, so neither the conservative JIT
>   scan nor the conservative interpreter-local probe fed `roots`; no injected
>   pin words), a root inside from-space that the walk did not record now
>   REFUSES the cycle through the incomplete-walk exit (decision
>   `skipped-young-walk-incomplete`, `note_skipped_young_cycle`'s floor):
>   nothing is copied or reset, so the root does not dangle.
> - `GenerationalHeap::unrecorded_young_root_refuses_cycle`: bounded. A cycle
>   refuses only when the lowest offending address differs from the one that
>   refused last (`last_unrecorded_root`, per heap), so a persistent stale
>   root costs ONE refused cycle and then the old fail-open exit applies (the
>   objection the "invalid-kind-or-element-tag" comment records against
>   failing closed). Each occurrence prints a rate-limited
>   `[unrecorded-young-root]` warn with the header words and the nearest
>   recorded start. Census `unrecorded_young_root_census()` =
>   `[cycles_refused, failed_open, roots_sum]`, zero on a healthy run.
> - The two fail-open exits the ledger did not record are now recorded like
>   the named three, serial (`forward_object_impl`) and parallel
>   (`gen_evac::ParEvac::evacuate`): `bad-forwarding-target` and
>   `extent-outside-from-space`. A post-GC verifier (`vm/src/memory/gc.rs`)
>   used to call those NEVER-OFFERED.
>
> **Still open (owner: the young-copy lane):**
>
> - (1) the producer, if any exists: none is known on this base.
> - The fail-open exit still applies to (a) the dirty-card slot values and
>   the overlay roots (read after the card take, so a refusal there would
>   lose card edges), and (b) any cycle with a live compiled frame, where
>   `roots` holds conservative words whose interior hits are refused BY
>   DESIGN. A refused precise root on such a cycle still dangles.
>
> **Verify (unit):** `cargo test -j 5 -p cratonvm-gc --lib gcd_d2h_tests` →
> 5 passed, among them
> `an_unrecorded_precise_root_refuses_one_cycle_then_the_cycle_runs`
> (census `[1, 0, 1]` then `[1, 1, 2]`) and
> `recorded_roots_leave_the_census_at_zero`; then
> `cargo test -j 5 -p cratonvm-gc --lib` (full lib, nothing else changes).
> **Runtime (the default path must be unchanged):** no `[unrecorded-young-root]`
> line on `cratonvm --java-home "$JDK" -XX:+UseGenerationalGC --nojit -Xmx64m
> -cp tools/bench GenR4W4EvacThroughputProbe 65536 20000000` (prints
> `PASS evac live=65536 iters=20000000 checksum=9065873663750453210 corrupt=0`),
> nor on the (A) arm below, 10/10:
> `CRATONVM_TLAB_SHARE_SIZER=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W3ConcUnloadProbe`
> → `conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true`.
> No `=0` switch: a precise root the walk did not record is the corruption
> this closes. Revert = drop the `unrecorded_young_root_refuses_cycle`
> conjunct at its one call site.
>
> *Previous status (2026-09-27, gcd d1/d), kept for the record:* UNCHANGED,
> not lane d's code; NOT the measured mechanism of the share-sizer's (A). The
> orchestrator's tripwire runs on the failing arm
> (`CRATONVM_DBG_ROOT_REMAP_AUDIT=1`, 4 reps) printed no `[forward-refused]`
> line.

*Filed 2026-09-27 by gen round 5, wave 6, lane `sizer10`, from reading while
hunting mechanism (A) of
`../../internal/gc/gengc-r5w4-orch-share-sizer-hands-out-live-memory-FIXED-20260928.md`. Not
reproduced in isolation.*

- **Status:** OPEN. Leading candidate for (A); not yet confirmed by a run.
- **Severity:** memory safety. A live object's holder is left dangling into
  memory that is reset and zeroed. It reads as null or class 0.
- **Backend:** Generational, the moving young path (`forward_object_impl`,
  `gen_evac.rs` parallel evacuation). Owner: the young-copy lane (pin).

## What is wrong

`GenerationalHeap::forward_object_impl` (`gc/src/gen_heap.rs`) has three
refusal exits that return `old_ptr` unmoved and record no `PointerMap`
entry:
- "not-an-object-start" (an address inside from-space that the pre-GC
  object-start walk did not see);
- "invalid-kind-or-element-tag";
- "suspect-header".

The parallel evacuator refuses the same way (`gen_evac.rs`, about
1606-1617). `update_value_ref` leaves a key missing from the map untouched,
so a STATIC root (`vm/src/memory/gc.rs` step 2, the `STATIC_REF_SLOTS`
fix-up) or any other root keeps naming from-space. The cycle then swaps and
resets from-space (`reset_deferring_zero` and the off-pause wipe), and the
root now points at zeroes. The object's children are never traced, so its
whole subgraph dies with it.

The code documents the second exit's fail-open as a decision
("Why it still fails OPEN"). Its argument is that an address on the exact
pre-GC grid with a bad tag is corrupt anyway. The FIRST exit gets no such
argument, and it is the dangerous one: a LIVE object is "not an object
start" whenever the walk skipped the span it sits in. That happens when it
is under a free block, a published reserved tail, or a filler written over
it (the first-word-only tripwires `FILLER_OVER_OBJECT` / `REFILL_OVER_OBJECT`
check the whole span only under `CRATONVM_DBG_DEADREF_STORE`).

Everything else in the moving path fails CLOSED: an incomplete walk, a
failed parallel chunk proof, or a reserved tail in from-space refuses the
whole cycle. The refusal exit is the one place a precise root can dangle.

## Why it fits (A)

The NPE text is derived from the bytecode ("... because
`GenR5W3ConcUnloadProbe.controlHolder` is null"). A class-0 receiver read
through a static that names zeroed from-space prints exactly that. The
concurrent old-gen sweep is exonerated (see the share-sizer page's r5w6
STATUS), and `OldGen::free` does not zero memory. The one zeroing that
fits a young `Payload` is this one.

## How to verify

`CRATONVM_TLAB_SHARE_SIZER=1 CRATONVM_DBG_ROOT_REMAP_AUDIT=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m --verbose:gc -cp tools/bench GenR5W3ConcUnloadProbe`,
JIT on, up to 10 reps. On a failing run, look for a
`[forward-refused] ... not-an-object-start` line whose address is the
`Payload` the static named (cross-check with `CRATONVM_DBG_OBJ_WATCH=Payload`).
The audit's nearest-recorded-start and walk report then name the span that
hid it.

## Proposed fix

1. **Diagnose the producer** (the span that hid a live object). That is the
   real bug.
2. **Make a refused root fail closed.** When a refusal is a "not-an-object-start"
   hit INSIDE from-space and the cycle is still before its copy phase, refuse
   the cycle (`start_walk_complete = false`, the same exit as T-3) instead of
   dangling. The worry that one corrupt word refuses every future cycle does
   not apply to this exit: the next cycle rebuilds the grid. Under the young
   lane's ownership; not landed here.
