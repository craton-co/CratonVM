# Proposal: the old-gen pinned compaction pins only the objects UNREWRITABLE words name

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 54
> of 54).** Not built. Its host switch `CRATONVM_GC_OLD_PINNED_COMPACT` is
> was flipped in `88b5b2dd7` and reverted (its true-root fallbacks regressed the OOME rows; triage.md "Final state"): on d7 old_pinned_compact
> and d3m_opc_1..3 print `jit-warm-humongous-after-fragmentation ok` / `PASS`
> while old_pinned_default fails. This page would pin fewer objects in that
> compaction; with layout P already passing, its gain is room, not
> correctness. **Gate:** `CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY=1` on
> `GenR4W5OldPinnedCompactProbe` with `made_room=true` and the JIT gauntlet
> with `CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1` silent. **Size:** M.

*Filed 2026-09-27 by gen round 5, wave 6, lane `old10`. A design, not a
defect; nothing here was built or measured. Owners: the JIT/root lane
(`vm/src/jit/conservative_roots.rs`, the non-moving door's post-GC remap)
for steps 1-2, the old-generation lane (`gc/src/gen_heap.rs`,
`OldPinnedCompact`) for step 3.*

## Why

`OldGen::compact_around_pins` (the non-moving path's answer to a
fragmentation request, `CRATONVM_GC_OLD_PINNED_COMPACT`, and by default for a
refused humongous allocation when nothing conservative is live) pins EVERY
object a published compiled-frame word names: `OldPinnedCompact::for_this_pause`
takes `gc_quiescence::pinned_jit_roots_snapshot()` — all band words of all
threads — as `words`. That is the only safe rule while nothing rewrites a
compiled frame after an OLD-generation move.

But the young collector already splits those words in two
(`conservative_roots.rs`, the band scan's movable/unrewritable partition):

* **movable** — a slot an oop map names, and the callee-saved GPR image and
  the deopt `SavedRegisters` GPR image (`register_image_remap_admits`): the
  moving young collection REWRITES these after it moves an object
  (`remap_active_jit_frames`, `remap_register_image_words`);
* **unrewritable** — the blind safepoint spill, operand spill above the live
  cursor, the outgoing/deopt reserve, XMM images
  (`gc_quiescence::add_unrewritable_jit_root`): pinned, never rewritten.

`GenR4W5OldPinnedCompactProbe`'s JIT-warm failure has two possible layouts
(`../../internal/gc/gengc-r4w6-review6-old-pinned-compaction-residuals-FIXED-20260928.md`, STATUS). In
layout **P** the live array B is named by a compiled-frame word and so pinned,
and the compaction cannot slide it into the dead A's hole. If that word is in a
movable class, the young collector would already have moved B and rewritten
the word; the old-gen compaction refuses only because the rewrite is not wired
for old-gen moves.

## Design

1. **Publication (JIT/root lane).** Keep the per-thread split for the
   old-gen pause: `pinned_jit_roots_snapshot` stays "every word" (G1 and the
   default humongous gate read it), and a second snapshot,
   `gc_quiescence::unrewritable_jit_roots_snapshot()`, returns only the
   unrewritable words of every thread (initiator, parked/blocked peers'
   deposits, take-over helper windows — the last are all unrewritable).
2. **Rewrite (JIT/root lane).** After a non-moving pause whose old-gen
   pointer map is non-empty, the door applies that map to every active
   compiled frame of every thread exactly as after a moving young
   collection: `remap_active_jit_frames(map, …)` for oop-map-named slots and
   the register-image remap for the callee-saved and deopt GPR images. A
   thread that cannot be remapped (a frozen in-JIT peer) already refuses the
   plan (`unrewritable_peer_state`). Confirm, and test, that a word in a
   movable class naming a MOVED old-gen object is rewritten
   (`CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1` reports none).
3. **The plan (old-gen lane), behind its own opt-in**
   (`CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY`): `for_this_pause` takes
   `words = unrewritable_jit_roots_snapshot()` instead of every word, and the
   root loop's `pin_root_range` rule is unchanged (peer-range and interpreter
   probe words stay pins). Every object a movable word names is then an
   ordinary precise root: marked by the root loop, slid by the compaction,
   remapped by step 2. Also keep the one-past-the-end probe for movable
   words (a derived end pointer is never oop-map-named, so it is
   unrewritable by construction; the probe stays on the unrewritable set
   only).

## What it does not fix

Layout **R** — a DEAD array retained because a word equals its base — is a
liveness problem, not a pin problem: the word is a marking root whatever its
class. That needs the JIT to stop publishing dead words (the cross-lane
request on the residuals page: dead-home clears for not-yet-defined homes,
a live-register `reg_mask` for the blind spill, dead callee-saved registers).

## How to verify

- Unit (old-gen lane): a pinned compaction over a heap whose movable word
  names an object above a hole slides it and returns it in the map; one
  whose unrewritable word names it keeps it in place.
- `CRATONVM_GC_OLD_PINNED_COMPACT=1 CRATONVM_GC_OLD_PIN_UNREWRITABLE_ONLY=1 RUST_LOG=cratonvm::gc=info cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx128m -cp tools/bench GenR4W5OldPinnedCompactProbe`
  → in layout P, `jit-warm-humongous-after-fragmentation ok` / `PASS`
  (HotSpot Serial's lines) and a compaction line with `made_room=true`,
  3/3.
- Negative control, the JIT gauntlet with both flags and
  `CRATONVM_DBG_VERIFY_OOP_MAPS=1 CRATONVM_DBG_JIT_STALE_AFTER_REMAP=1`: no
  stale-word report naming an old-gen address; outputs unchanged.
