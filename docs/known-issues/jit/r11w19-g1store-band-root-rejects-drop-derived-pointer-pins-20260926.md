# G1 band-root rejects also drop the region pin a derived pointer may depend on

Status: OPEN
Area: `gc/src/g1.rs` (`is_object_address_inner`, `is_object_address_for_root_scan`),
`vm/src/memory/roots.rs` (G1 pin publication), JIT frame layouts / oop maps
Severity: medium (latent; no reproducer)
Found by: round 11 wave 19 lane g1store

## What is wrong

Under G1 the own-thread band scan's accepted words are BOTH roots and pins: `vm/src/memory/roots.rs`
publishes `roots[jit_scan_start..]` through `gc_quiescence::publish_pinned_jit_roots`, and
`G1Collector::jit_pinned_region_set` keeps each address's region out of the collection set. A word
the predicate rejects is neither. Three rejects in the predicate hit interior words of LIVE objects:

* wave 10: every word in a `HumongousContinuation`, and every non-base word of a `HumongousStart`
  (`is_object_address_inner` ~26189);
* wave 12: a decoded extent that crosses the region end (`is_object_address_for_root_scan`);
* wave 14: a decoded extent that runs past the cursor (`root_scan_end_is_below_cursor`).

For a derived pointer (an array element cursor, a field address kept in a saved register) whose
base is NOT also in a scanned slot, the reject means: an Eden/Survivor object may be evacuated under
it (its region is no longer pinned by this word), and a humongous object may be eagerly reclaimed
(`eager_reclaim_humongous_locked` resolves only words that reached the root array). The
generational heap's exact answer has the same exposure for liveness, but keeps interior words as raw
pins in its young pin-word ledger ("nothing rewrites a derived pointer",
`vm/src/jit/conservative_roots.rs` `young_pin_frame_screen_...`), and `gen_heap.rs` ~12192 credits
G1's REGION-granularity pins with absorbing the gap. The rejects above remove that absorption for
exactly the words they reject.

Nothing measured shows a derived pointer without its base across a GC-capable call. The w16/w18
corrupt-cell investigation proved `obj + 8` words exist in compiled frames' saved-register images;
whether the base is always in another scanned slot at the same safepoint is a JIT question nobody
has answered.

## Proposed fix (either)

1. Split roots from pins for G1: publish the region of every in-arena, 8-aligned band word
   (`VmHeap::is_heap_addr`) as a pin, and keep `is_object_address_for_root_scan` for the root
   array only. Cost: more pinned regions per pause (stale in-arena words pin too); measure
   `jit_pinned_out` on `R11HashmapAastoreExact` and a Tomcat run before and after.
2. Prove the JIT invariant: a verifier arm (debug builds) that, at each safepoint map, checks that
   every register or spill slot the frame layout marks as possibly holding a derived value also has
   its base named by the map. That needs derived-pointer provenance in `FrameLayout`, which does not
   exist today.

## How to confirm

A Java probe that walks a humongous `long[]` in a compiled loop whose only live reference is the
element cursor across an allocating call (hard to force from Java; the JIT decides register
allocation). More practical: `CRATONVM_DBG_JIT_ROOTSCAN=1` under G1, log band words that the
predicate rejects but `is_heap_addr` accepts, and check whether each one's containing object is
named by another root of the same pause.

## Round 12 wave 1 (lane g1store)

Landed (proposal W19-1, split roots from pins; pending orchestrator build):

* `G1Collector::band_reject_pin(addr)` (`gc/src/g1.rs`): for a word in a live
  region, the pin it owes. A humongous word (start region or continuation)
  pins the span BASE, so the pin lands on the `HumongousStart` region exactly
  where an accepted base root lands. Any other word pins itself (its region).
* `conservative_roots::g1_band_reject_pins` (`vm/src/jit/conservative_roots.rs`):
  a per-thread capture. While a caller has it armed, `scan_one_frame` and
  `scan_one_frame_filtered` record every word the root screen rejects that
  `band_reject_pin` places (for the filtered scan, only words that would have
  been roots). The words never enter the root array.
* `memory::roots::collect_roots` arms it around the initiator's JIT scan and
  appends the pins to its `publish_pinned_jit_roots` publication
  (`publish_g1_band_reject_pins`). Humongous pins are published by default
  (`CRATONVM_G1_BAND_REJECT_PINS`, `=0` off); Eden/Survivor/Old pins only under
  `CRATONVM_G1_BAND_REJECT_PINS_ALL=1`, because each keeps a whole region out
  of the collection set. `CRATONVM_DBG_JIT_ROOTSCAN=1` prints a
  `[jitpins-rejects]` line per collection with the word counts of both kinds
  and the regions they name that no accepted root pins. That is the pricing
  the proposal asked for, printed whether or not `_ALL` is on.
* Two consumers now honour the pins where they did not before:
  * `jit_pinned_region_set` reads the snapshot when a compiled frame is live
    by either detector (`is_active()` or the scan's
    `unregistered_jit_frame_on_stack`), not by `is_active()` alone. An A5-only
    pause (compiled frame with no `JitEntryGuard`) ignored every pin and
    evacuated the objects the frame's raw slots named. Kill switch
    `CRATONVM_G1_PINS_HONOUR_UNREGISTERED_FRAME=0`.
  * `cleanup` no longer frees in place an Old region or a humongous span the
    pin set names (`cleanup_jit_pinned_regions`). A rejected word is never a
    mark root, so an object only a derived pointer names is unmarked and was
    freed at cleanup while the frame still used it. Kill switch
    `CRATONVM_G1_CLEANUP_HONOURS_JIT_PINS=0`.
* Tests: `r12w1_a_rejected_humongous_interior_word_pins_its_span`,
  `r12w1_an_unregistered_compiled_frame_alone_still_pins_its_roots`,
  `r12w1_cleanup_keeps_a_humongous_span_a_compiled_frame_pins` (`gc/src/g1.rs`),
  `r12w1_g1_band_rejects_are_captured_as_pins_not_roots`
  (`vm/src/jit/conservative_roots.rs`).

Still open, so the Status stays OPEN:

1. The two deposit paths (parked and blocked peers) do not arm the capture yet:
   `r12w1-g1store-deposit-paths-arm-reject-pins-patch-FIXED-20260926.md` holds the
   exact patch. Until it lands, only the initiator's own frames are covered.
2. Eden/Survivor/Old pins are opt-in. Flip `CRATONVM_G1_BAND_REJECT_PINS_ALL`
   only after `[jitpins-rejects] new_regions=` has been read on
   `R11HashmapAastoreExact`, the Tomcat set and `probes/TvmProbe.java`
   (the H2 row that made region pins expensive).
3. The mark side for non-humongous objects: a derived-only Old object is
   unmarked; `cleanup` now keeps its region only while a frame pins it at
   cleanup, and a mixed pause copies only what roots and remembered sets
   reach. `remark` could take the captured words as mark roots
   (`mark_root_seed` already places an interior word in its containing
   object); see `jit-r12-g1store-proposals.md` W1-2.
4. No reproducer. The JIT's IR has no derived-pointer value (every array or
   field address is formed inside one lowering and dies there), so the words
   come from Rust helper frames under the whole-band fallback, or from stale
   stack. `R12G1storeDerivedPin` stresses humongous arrays through natives and
   compiled loops under `-XX:+UseG1GC`; a wrong checksum there, or a
   `[jitpins-rejects] humongous=(words>0` line, is the first evidence either
   way.

## Round 12 wave 2 (lane g1store)

The deposit-path patch (`r12w1-g1store-deposit-paths-arm-reject-pins-patch`) is
in the tree: `update_root_snapshot` and the blocked deposit arm the capture, so
parked and blocked peers publish their reject pins too (item 1 above is done).

Landed (proposals W1-2 and W1-3; pending orchestrator build):

* **The mark half.** `G1Collector::remark` (`gc/src/g1.rs`) now seeds the gray
  set from every published JIT pin (`gc_quiescence::pinned_jit_roots_snapshot`)
  after the roots, through the same placement a root gets
  (`mark_root_seed`: a start seeds itself, an interior word its containing
  object, a humongous word its span's object), in both remark calls (initial
  mark and final remark). New `G1Collector::jit_pin_mark_seeds`. What it fixes:
  an object only a pin kept (a humongous span a compiled frame reaches through
  an element cursor, or under `_ALL` an Old object reached through a derived
  word) was kept by wave 1's cleanup pin check but left UNMARKED, so (a) the
  objects it references were unmarked and an Old region holding only such
  referents was freed in place at cleanup, and (b) remark-time reference
  processing (`is_live_after_mark`) read the object and its referents as dead,
  clearing weak references to strongly reachable objects and queueing their
  finalizers. A pin the grid cannot place seeds nothing and sets
  `mark_saw_implausible`, the cleanup fail-safe an unplaceable root takes
  (cleanup would keep its region, but not what its object references).
  Gated on `jit_frames_live_for_pins()` like every other pin consumer. Kill
  switch `CRATONVM_G1_REMARK_SEEDS_JIT_PINS=0`.
* **The census.** The same pass counts, per remark, distinct pins that a root
  already covers, pins placed inside an object, pins that name nothing or
  could not be placed, and ORPHANS: pins whose object no root of the pause
  names (split into humongous and interior). `tracing::debug` always; under
  `CRATONVM_DBG_JIT_ROOTSCAN=1` one stderr line per remark:
  `[g1-jitpin-census] pins= covered= interior= nothing= unresolved= orphans= orphan_humongous= orphan_interior=`.
* Tests: `r12w2_jit_pin_census_tells_covered_pins_from_orphans`,
  `r12w2_remark_marks_what_only_a_jit_pin_names` (fails before the change: the
  pinned humongous holder and the Old object only it references read dead
  after the mark).
* **Every pin consumer's gate** (`jit_frames_live_for_pins`: the evacuation
  pin set, cleanup, the remark seeding above) now also opens when any thread
  has a pin published. Both detectors answer for the initiator only
  (`is_active()` counts guarded entries; the A5 flag is the initiator's own),
  so a PEER parked or blocked in a compiled frame without a `JitEntryGuard`,
  with no guarded entry anywhere, published pins every pause ignored. Kill
  switch `CRATONVM_G1_PINS_HONOUR_ANY_PUBLICATION=0` (with it on,
  `CRATONVM_G1_PINS_HONOUR_UNREGISTERED_FRAME=0` alone no longer restores the
  round-11 gate; set both). Test
  `r12w2_a_published_pin_is_honoured_with_no_detector_on_the_initiator`.
  The shared-gate refactor (W1-5) is the patch page
  `r12w2-g1store-one-compiled-frames-live-gate-patch-FIXED-20260926.md`.
* Probe: `C:\craton\jitr12-probes\src\R12G1storeWeakUnderPin.java` (a
  WeakReference to an element of a humongous array the compiled loop is
  walking must never read null).

### Measurement plan: can `CRATONVM_G1_BAND_REJECT_PINS_ALL` default on?

The two costs and the one benefit are now all printed by one switch pair.
Run each of `R11HashmapAastoreExact`, `R12G1storeDerivedPin`,
`R12G1storeWeakUnderPin`, `probes/TvmProbe.java` (H2) and one Tomcat run under
`-XX:+UseG1GC` with `CRATONVM_G1_BAND_REJECT_PINS_ALL=1 CRATONVM_DBG_JIT_ROOTSCAN=1`,
and collect:

1. Need: the sum of `orphan_interior=` over all `[g1-jitpin-census]` lines
   (non-humongous objects named only through a rejected word). Under `_ALL`
   those words are published, so this is the count of derived-only objects the
   default (region pins off) leaves unmarked at remark and unpinned at pauses.
2. Cost: `[jitpins-rejects] region=(words=W new_regions=R ...)` per pause; the
   fraction of pauses with `R > 0` and the mean `R` (each is a whole region kept
   out of the collection set), plus the pause-time and `jit_pinned_out` delta
   against the same run without `_ALL`.
3. Health: `unresolved=` on the census lines (placements the grid could not
   make; with `_ALL` these are raw interior words). Each non-zero line costs
   that cycle's cleanup reclamation (`g1 cleanup: implausible gray entry seen`
   warn), so its rate is part of the `_ALL` price.

Decision rule: flip `_ALL` on by default only if (1) is non-zero somewhere in
the battery (a real derived-only object exists) and (2) shows `new_regions`
usually 0. If (1) is zero everywhere, leave `_ALL` off: the JIT's IR has no
derived-pointer value, and the only non-humongous rejects are then stale or
helper-frame words that name objects some root already names. Either way the
mark side needs no flip: it is on by default and seeds whatever is published.

Still open (Status stays OPEN):

1. The measurement above (needs runs).
2. Without `_ALL` a non-humongous reject word is neither pinned nor marked. A
   mark-only publication (seed at remark, never pin) would close the mark half
   for them at no CSet cost; it needs a second per-thread publication in
   `gc_quiescence` (proposal W2-1 in `jit-r12-g1store-proposals.md`).
3. Young/mixed pauses still decide only by pins and roots; the census runs at
   remark only (proposal W2-2 extends it to evacuation pauses).

## Round 12 wave 8 (lane rt3)

The assignment was "fix or prove unreachable". I proved one half by reading,
changed no code, and the Status stays OPEN for the other half.

**Proved: compiled code never holds a derived pointer across a GC-capable
point, in either tier.**

* Optimizing tier: `jit/src/ir.rs` `enum Op` has no address-valued op. `Load` /
  `Store(MemKind)` take the object, and `ArrayLoad` / `ArrayStore(MemKind)` take
  `(array, index)`. `ir_lower` folds each into one addressing-mode operand of a
  single instruction, and the register allocator only ever sees Java-typed
  values. A derived value therefore does not exist in the IR, let alone live
  across a call or a poll. The page's wave-1 note ("the JIT's IR has no
  derived-pointer value") holds on the wave-7 tree.
* Single-pass tier: operand-stack slots and locals hold Java values. Its
  loop-invariant hoists (`jit/src/x64/licm.rs`) keep an `arraylength` (an
  `int`), a row of an `Object[][]` and the X2 field array: object STARTS, which
  the root screen accepts. Element and field addresses are formed inside one
  bytecode's lowering. The only helper that receives an interior address is
  `jit_write_barrier`, under the generational card view (precise `aastore`
  cards, "gen r4w4/cards4" in `helpers.rs`), and the emitter passes one only
  when the helper table carries a generational card view: never under G1. That
  helper also does not reach a safepoint. The G1 post barrier
  (`jit_g1_post_write_barrier`) receives the object start. An inline-TLAB
  cursor word left in a register across the `new_object` slow call points at
  unallocated Eden, which is not a derived pointer to a live object; pinning it
  under `_ALL` is harmless.

So under G1 a band word from a COMPILED frame that the root screen rejects is a
stale word: a dead spill, a callee-saved leftover, or a return-address
neighbour. It is never a live cursor the frame will dereference after the
pause.

**Not proved (why the Status stays OPEN):** the band also covers Rust helper
frames under the whole-band fallback (`conservative_roots.rs`
`scan_one_frame`, and the A5 unregistered-frame sweep). A VM helper that holds a
raw `*mut u8` into an array or object while it allocates or reaches a safepoint
leaves a derived word there, whose base may sit in no scanned slot. That is a
question about Rust VM code under any moving collector, not about the JIT, and
reading every helper is outside this lane.

What is left, exactly:

1. The measurement battery in the wave-2 section, unchanged. Read
   `orphan_interior=` on the `[g1-jitpin-census]` lines under
   `CRATONVM_G1_BAND_REJECT_PINS_ALL=1 CRATONVM_DBG_JIT_ROOTSCAN=1`. By the
   argument above, a non-zero value can only come from a Rust helper frame or a
   stale word, so a non-zero orphan should be traced to its frame before
   anyone flips `_ALL` on. The reject capture (`g1_band_reject_pins::note`)
   keeps no slot address today, unlike an accepted root's provenance
   (`gc_quiescence::record_jit_root_provenance`); recording `slot` there is the
   first step of that trace.
2. W2-1, the mark-only publication (seed at remark, never pin), needs a second
   per-thread publication in `gc_quiescence` (a GC file; not this lane's).
3. Close this page once (1) reads zero orphans across the battery: at that
   point the latent case is shown unreachable in practice as well as for
   compiled code.

## Round 14 wave 1 (lane codecache)

Verified on `adb9178bc`: the JIT half of round 12 wave 8's proof still holds. `jit/src/ir.rs`
`enum Op` (665-1196) has no address-valued op (the only additions since wave 8 are Java-typed:
`LambdaIntToDouble`, `ScalarIntrinsic`, `Unbox`, `ExactClassIs`, the three `Osr*` seeds, whose
`OsrMemory` reads a Java value out of the interpreter's locals array inside one lowering). No change
to the single-pass tier's hoists that would keep an element or field address across a call or poll
was found (`jit/src/x64/licm.rs` hoists `arraylength`, round 13 wave 3's `int` field hoist
`IntFieldHoist`, rows and field arrays: `int`s or object starts; `cdc12792c` only narrowed which
words are roots).

So nothing here is a JIT defect, and this lane has nothing to fix. What is left is a GC measurement
(the `orphan_interior=` battery in the wave-2 section) and a question about Rust VM helper frames
under the whole-band fallback. Recommend the orchestrator re-home the page under
`docs/known-issues/gc/` (the GC round is closed; it is that session's next item) rather than keep it
in the JIT round's open count. Status unchanged: OPEN.
