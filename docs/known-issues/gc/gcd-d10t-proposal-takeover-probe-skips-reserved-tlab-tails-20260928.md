# Proposal: the take-over word probe should not root words inside a frozen peer's own reserved TLAB tail

*Filed 2026-09-28 by gcd d10/t (lane takeover10). A proposal, not a defect:
nothing is wrong with the current root set, it is only wider than it needs
to be.*

## What happens today

On Generational and G1 the take-over probe (`xt_root_scan::takeover_word_probe`
-> `VmHeap::resolve_interior_for_pin` -> `is_heap_addr`) answers ANY 8-aligned
word inside an arena with the word itself, and `takeover_word_companion` then
roots the word below every such echo. A peer frozen in compiled code holds
several words inside its OWN unallocated TLAB tail: the uncommitted new cursor
of an inline bump (`RAX`), the TLAB `end` a Rust refill left in a
caller-saved register or a dead spill slot, and stale words naming cells a
non-moving sweep freed and a refill re-carved. gcd d10/t proved these name no
object (`../../internal/gc/gcd-d8x-tlab-skip-span-guard-sees-roots-inside-published-spans-FIXED-20260929.md`)
and made the skip-span guard stop reporting them; they are still ROOTS:

- Generational: `mark_young`'s anchor oracle drops them as gap space (the
  published span), so they only cost oracle lookups.
- G1: each is a conservative frozen-peer root that `pin_frozen_peer_roots_for_g1`
  pins, keeping the TLAB's region (and, for `end - 8` at a region boundary,
  possibly the next one) out of the collection set for the pause.
- ZGC: `resolve_interior_for_pin` resolves to a containing base, so an
  unallocated-tail word resolves to nothing or to the object below the tail's
  start; minor.

## Proposal

After `collect_reserved_tlab_tails` (which already runs in the same pause, with
every owner stopped) and before the roots reach the collector, drop every
take-over root that lies STRICTLY inside a published span whose memory reads
unallocated (`cratonvm_gc::heap::judge_skip_span_memory` is the exact test and
is already written). Keep a root exactly at a span start (the in-flight object
a peer frozen before its commit is building; its body is zero, so keeping it
costs nothing and dropping it proves nothing).

Where: a filter in `gc_and_alloc.rs::stw_take_over_and_wait` right after the
span publish (lane o's region), fed by a `pub fn` in `gc/src/heap.rs` that
wraps `judge_skip_span_memory` over the span list. Opt-in first
(`CRATONVM_XT_TAKEOVER_DROP_TAIL_WORDS`), measured by
`skip_span_root_counts()`'s `unallocated` bucket falling to zero and by G1's
pinned-region count on `GenR4W5ConcMarkJitGateProbe`.

## Why not now

It removes roots, which is the unsafe direction if the span proof is ever
wrong; the guard that would then catch a real stale span sees exactly these
roots. One verification cycle of the exact guard (d10/t) on all three
collectors should come first.
