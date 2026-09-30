# On the default collector a short-lived compiled allocation costs ~25 ns more than on G1: about 100 of the ~140 ns per `CratonBench hashmap` put+get pair (GC round)

Status: OPEN (for the GC round; the JIT half is landed, default OFF, see below)
Area: GC: the generational collector's young cycle on a JIT-warm process (`gc/src/gen_heap.rs` `collect_garbage_inner_with_pins` term 4 of `divert_non_moving`, `sweep_young_non_moving`, `refill_tlab` / `refill_fragmentation_fallback`)
Severity: MEDIUM (performance: the largest single term of `CratonBench hashmap`'s ~2.8x)
Found by: round 13 wave 2 lane irhash (hand-off of the GC half of `r12w7-iropt6-compiled-allocation-premium-on-jit-warm-gengc-20260927.md`)

## What is wrong

`R12Iropt5HashmapSplit` on the round-12 w6 binary (ns per operation): `fresh-get` minus
`prebox-get-same` -- one inline-bumped `Integer` that dies at once, same compiled code in
both arms -- is **29 ns on GenGC and 4 ns on G1**. A put+get pair of the kernel allocates
four such objects (two boxes and a `Node` in the put, one box in the get), so about 100 of
the ~140 ns per pair that separate CratonVM from HotSpot is this premium, more than every
JIT term together (the two remaining compiled calls are ~14 ns; see
`r13w2-irhash-putval-getnode-need-callee-frame-states-FIXED-20260929.md`).

The mechanism is the GC round's own finding, not re-derived here:
`docs/known-issues/gc/gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-20260926.md`
(on a JIT-warm process no young cycle copies; the in-place sweep fragments young and TLAB
refills are carved from holes) and its proposal
`gengc-r5w3-evac7-proposal-default-the-parallel-pinned-young-copy-20260926.md`. This page
only adds the `hashmap` evidence and the measurement the GC round should take.

## What the JIT side did (round 13 wave 2)

The optimizing tier's `aastore` into an old table (`tab[i] = newNode(..)`) no longer calls
`jit_aastore` when the element's card is already dirty, under
`CRATONVM_JIT_INLINE_CARD_MARK=1` (default OFF, awaiting the GC round's A/B of that switch;
kill switch `CRATONVM_JIT_IR_INLINE_CARD_CHECK=0`). That is the only barrier term the JIT
owns on this path.

## How to confirm / what would close it

Interleaved, on one binary, in the arms default / `-XX:+UseG1GC` /
`CRATONVM_GEN_PINNED_YOUNG_COPY=1` / `CRATONVM_GEN_YOUNG_PIN_LEDGER_TERM4=1`, each with and
without `CRATONVM_JIT_INLINE_CARD_MARK=1`:

- `C:\craton\jitr12-probes\src\R12Iropt5HashmapSplit.java` (`fresh-get` - `prebox-get-same`,
  `node-only`, `kernel`);
- `C:\craton\jitr12-probes\src\R12Iropt6StoreAllocSplit.java` (`ring`, `youngtable`,
  `oldstore`, `node`);
- `C:\craton\jitr13-probes\src\R13IrhashOldTableStore.java` (`fill-churn`; `bad 0` in every arm);
- `CratonBench hashmap`.

Closed when the GenGC `fresh-get - prebox-get-same` difference is within a few ns of G1's
in the default arm.
