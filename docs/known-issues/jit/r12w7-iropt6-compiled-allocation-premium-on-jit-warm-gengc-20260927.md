# A short-lived compiled allocation costs ~25 ns more on the default collector than on G1, and allocate-and-store costs ~210 ns everywhere

Status: OPEN (measured on the w6 matrix; attribution by reading, confirmation needs the new probe)
Area: the generational collector's young cycle and TLAB refill (`gc/src/gen_heap.rs` `collect_garbage_inner_with_pins` term 4, `sweep_young_non_moving`, `refill_tlab` / `refill_fragmentation_fallback`); the IR `aastore` barrier path (`jit/src/ir_lower.rs` `Op::ArrayStore(Ref)`, `emit_gated_ir_aastore`)
Severity: MEDIUM (performance: the largest single ingredient of `CratonBench hashmap`'s gap, about 100 of ~139 ns per put+get pair)
Found by: round 12 wave 7 lane iropt6 (reading the w6 probe matrix)

## What the numbers say

`R12Iropt5HashmapSplit`, w6 binary, ns per operation (524 288 per phase):

| phase | GenGC (default) | G1 | HotSpot |
|---|---|---|---|
| `prebox-get-same` (2 compiled calls, identity hit) | 15 | 17 | ~0 |
| `fresh-get` (the same + one inline-bumped `Integer` + the `equals` prefix) | 44 | 21 | ~0 |
| `box-only` (the box scalar-replaced) | 0 | 0 | 0 |
| `node-only` (allocate a 4-field object, store it into a table allocated before the run) | 213 | 303 | 4 |

- `fresh-get - prebox-get-same` is the box: **29 ns on GenGC, 4 ns on G1**,
  with the same compiled code (the IR `Integer.valueOf` prefix bumps the TLAB
  inline and stores one field; `R11W17HashmapValueOfInline` pins it). The box
  dies at once.
- `node-only` is ~210 ns in the default, `C2_ACCEPT=never`, `nle0`,
  `--compatible` and G1 arms alike, so it is neither a tier nor a collector
  choice.

## Why, by reading (hypotheses to confirm)

1. **On GenGC a JIT-warm process never copies young**
   (`gc/src/gen_heap.rs`, `unrewritable_conservative_jit_roots`, term 4 of
   `divert_non_moving`: "any thread is in compiled code AND somebody ran a
   conservative scan this cycle", which "is positive whenever anything is
   compiled"). The in-place sweep leaves survivors where they were, the free
   list fragments, and TLAB refills after it are carved from holes
   (`refill_fragmentation_fallback`, floor 256 B), each hand-out zeroed again.
   The GC lane's page
   `docs/known-issues/gc/gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926.md`
   derives the same for `GenR4W4EvacThroughputProbe`. A 16-byte box then pays
   a share of a refill plus the per-cycle O(allocated) sweep. G1 copies, so
   its TLABs stay large.
2. **`node-only` adds survival and an old-array store.** Every stored object
   survives a round, so each young cycle retains ~2.6 MB in place (GenGC) or
   copies it (G1); and every `table[i] = c` into the old table is a
   `jit_aastore` Rust call in the optimizing tier (the gated inline store
   declines on the array's flags byte whenever the barrier may have work) and a
   `write_barrier` call in the single-pass tier unless
   `CRATONVM_JIT_INLINE_CARD_MARK=1`.

## How to confirm (orchestrator)

`C:\craton\jitr12-probes\src\R12Iropt6StoreAllocSplit.java` separates the
ingredients: `ring` (allocate, young holder, dies at once), `oldstore` (no
allocation, old objects into the old table: the barrier alone), `node` (the
node-only shape), `youngtable` (allocate into a per-call table). Run it, and
`R12Iropt5HashmapSplit` and `CratonBench hashmap`, in these arms:

- default; `-XX:+UseG1GC`;
- `CRATONVM_GEN_PINNED_YOUNG_COPY=1` and `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1`
  (the two GenGC switches that let a JIT-warm young cycle copy);
- `CRATONVM_JIT_INLINE_CARD_MARK=1` (single-pass inline card check; the
  optimizing tier has no twin, so compare with `CRATONVM_C2_ACCEPT=never`).

Reading: if `ring` and `youngtable` fall to HotSpot's level under the copy
switches, hypothesis 1 owns the box premium; if `oldstore` alone is tens of ns,
the store barrier owns `node-only` and proposal W7-2 (an IR twin of the inline
card check) is worth building.

## Proposed fix

Not in the JIT mid-end:

- GC lane: default one of the pinned young copy / pin-ledger term-4 paths for
  JIT-warm processes (their own pages carry the soundness argument), or make
  the in-place sweep's allocation cheap (no re-zeroing, larger refills).
- JIT, if `oldstore` shows the barrier: proposal W7-2 in
  `jit-r12-iropt-proposals.md` (the optimizing tier's inline card check, behind
  the existing `CRATONVM_JIT_INLINE_CARD_MARK`).

## Round 13 wave 2 (lane irhash): the JIT half (W7-2) landed, default OFF

`jit/src/ir_lower.rs` `emit_gated_ir_aastore` + the new `emit_ir_aastore_card_arm` /
`ir_inline_card_view`: where the gates say the post barrier MAY have work (a non-null value
into a flagged-old array while old objects exist) the optimizing tier used to call
`jit_aastore` on every store. It now runs the single-pass tier's generational card CHECK
first (`x64/objects.rs` `emit_gen_card_check`, the same view, offsets and range tests):

- array outside the view's old range: the helper (the collector decides);
- value in the old range: old -> old, stored inline with no card;
- otherwise the card of the ELEMENT (element-precise tables) or of the array header: clean
  -> the helper, exactly as before; dirty -> the SATB `pre` byte is re-tested, the element
  is stored inline, and the card is tested again (POST); a card a collection cleaned in the
  meantime sends the store to `jit_aastore`, which stores the same value again (idempotent)
  with the full barrier.

Compiled code never writes a card byte, and every "not sure" edge is the helper; the
BUG-03 takeover argument is the single-pass one (`emit_gen_card_barrier`), written out on
the function. Enabled only by the single-pass opt-in `CRATONVM_JIT_INLINE_CARD_MARK=1`
(default OFF, awaiting the GC lane's A/B, so one A/B switches both tiers), with this tier's
own kill switch `CRATONVM_JIT_IR_INLINE_CARD_CHECK=0` (default ON). Default builds are
byte-for-byte unchanged except that the gated store's exit jump is now one element of a
`Vec`. Needs the plumbing patch `r13w2-irhash-ir-card-view-plumbing-patch-FIXED-20260928.md`
(a `Lowerer` field and four re-exports lane irhash does not own).

Test: `ir_lower.rs` `mod tests` `r13w2_irhash_a_dirty_card_spares_the_aastore_helper`
(executed against a real `CardTable` over a simulated old space: clean card -> helper,
dirty card -> inline, old value -> inline with the card left clean, armed SATB -> helper).
Probe: `C:\craton\jitr13-probes\src\R13IrhashOldTableStore.java` (young nodes stored into an
old table, young collections in between, every node read back and checked inside the hot
method), plus `R12Iropt6StoreAllocSplit` `oldstore` / `node` for the timing.

What is left on this page is the GC half (the young-cycle allocation premium on a JIT-warm
GenGC process), already filed for the GC round as
`docs/known-issues/gc/gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926.md`
and its proposal `gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy-20260926.md`;
the hand-off page with the `hashmap` evidence and the measurement that would close it is
`r13w2-irhash-gengc-young-premium-is-most-of-the-hashmap-gap-20260927.md`. For the flip of `CRATONVM_JIT_INLINE_CARD_MARK` the
orchestrator should A/B `CratonBench hashmap`, `R12Iropt5HashmapSplit` (`prebox-put`,
`node-only`, `kernel`) and `R13IrhashOldTableStore` (`fill-churn`).

Stays OPEN (the GC half, and the flip).

## Round 13 wave 8 (lane iropt7): two more JIT-side terms on the `Node` allocation

Found by reading the `Op::New` arm against `putVal`'s compile (pending build):

- **A `new` in a spliced body paid a `jit_post_tlab_init` call on every allocation.** `lib.rs`
  fills `tlab_announce_classes` -- the set that makes an IR `new` skip the post-init helper and
  sink its safepoint map onto the slow path -- from the compiling method's OWN `new` sites only.
  `new Node` in `putVal` comes from the guarded `newNode` splice, so every put took the map
  publication before the bump, the helper call (panic guard, JIT-boundary note, layout lookup,
  flag re-store, recipe probe), the frame republish and the shadow reload. Now a spliced site gets
  the same elision when its class is proven initialised, not finalizable (the splice resolver's
  own refusal) and has no `long`/`float`/`double` instance field (read off its compact layout):
  `ir_lower.rs` `spliced_new_post_init_is_noop`, `CRATONVM_JIT_IR_SPLICED_NEW_SKIP_POST_INIT`
  (default ON). This is a per-allocation Rust call on the put path, so part of what this page
  attributed to "allocation on the default collector" may have been this call: re-measure
  `R12Iropt5HashmapSplit` `prebox-put` / `kernel` with the switch `0` and `1`.
- **The inline fast path's join.** It now skips the identity republish and the null test that only
  the stub's return needs (`CRATONVM_JIT_IR_ALLOC_FAST_JOIN`, default ON).

Neither touches the GC half, which stays with the GC round. Details:
`r13w8-iropt7-hashmap-kernel-splice-status-CLOSED-20260929.md`.
