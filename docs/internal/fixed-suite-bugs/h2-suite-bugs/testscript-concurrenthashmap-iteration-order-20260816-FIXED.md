# ✅ FIXED — `ConcurrentHashMap` iteration order: the reorder was masking with OUR capacity, not the JDK's

## Status
**RESOLVED 2026-08-17** on branch `fix/h2-locale-collation-chm-order-20260817`.
The four `testScript.sql:6425` errors are gone; `org.h2.test.scripts.TestScript`
reports **0 errors** under CratonVM, the same as HotSpot JDK 25.

Filed originally as: *`ConcurrentHashMap` iterates in a different order than the
JDK's, and `SCRIPT` output inherits it.*

The original record priced the fix as "reimplement the JDK's `ConcurrentHashMap`
table layout … not small", and deferred it. That estimate was too pessimistic by
a long way: **the reorder already existed** — `chm_reorder_by_virtual_bucket`,
added for a Spring alias-ordering fix — and it was masking with the wrong
number. The change is one expression plus the constructor bookkeeping it needs.

## Root cause

`chm_reorder_by_virtual_bucket` sorted each entry by
`hash & (chm_total_capacity(this) - 1)` — the sum of the **live segment bucket
arrays**. That looks like "what a flat table of the same size would be" and is
not:

> our segments resize on their OWN load; a real flat table resizes on TOTAL
> load.

They agree at construction and drift apart from the first segment resize onward.
For the eight constraint names in this very test the sum was **32** where the
real JDK table was **16**, and a mask of 31 reproduces exactly the order the
test saw:

| table size | resulting order |
|---|---|
| 16 | `C3, CONSTRAINT_760, MIN_LENGTH, DATE_UNIQUE, CONSTRAINT_76, DATE_UNIQUE_2, B_UNIQUE, CONSTRAINT_7` — **HotSpot** |
| 32 | `CONSTRAINT_76, DATE_UNIQUE_2, CONSTRAINT_7, C3, CONSTRAINT_760, MIN_LENGTH, DATE_UNIQUE, B_UNIQUE` — **CratonVM, as filed** |

The original record's own diagnosis — "not a hashing difference: it is the map's
own bin layout" — was correct, and `String.hashCode` really is identical on both
VMs. The bin layout was one number wide.

## The table-size function is measured, not derived

`probes/ChmTableSizeProbe` reads `ConcurrentHashMap.table.length` by reflection
for every initial capacity in `1..=80` and for a default map at every size to
200. The function that fits all of it:

> the smallest power of two `P` with `n < P - (P >>> 2)`

Spot values: `CHM(2)`→4, `CHM(3)`→8, `CHM(11)`→16, `CHM(12)`→32, `CHM(47)`→64,
`CHM(48)`→128; a default map grows 16→32→64→128 as the size reaches 12, 24, 48.

This is **not** `tableSizeFor(c + (c >>> 1) + 1)`, which the constructor's source
reads like and which predicts 32 for `CHM(11)`. The real map holds 16. Where the
two disagreed the measurement won, and three unit tests pin the readings.

The constructor's capacity is recorded at construction in `sizeCtl` — the JDK's
own field for exactly this (`ConcurrentHashMap(int)` ends with
`this.sizeCtl = cap`), an `int` field on the real class so no descriptor
coercion applies.

## The intra-bucket tie-break needed no new state

Two keys in one virtual bucket share every low bit of the mask, so they also
share our segment and (our per-segment table never being larger than the virtual
one) our bucket — one chain. `probes/ChainOrderProbe` settles what that chain's
order is using **equal-hashCode keys** (`"Aa"`/`"BB"`, the four products of
`"AaAa"`…`"BBBB"`), which no table size can separate, so whatever comes out is
the chain order and nothing else:

```
inserted=AaAa AaBB BBAa BBBB | iterated=AaAa AaBB BBAa BBBB   HotSpot
inserted=AaAa AaBB BBAa BBBB | iterated=AaAa AaBB BBAa BBBB   CratonVM
```

Insertion order on both, already. A stable sort therefore gets the tie-break
right for free, which is why no per-node sequence number was needed.

## Result, measured

`probes/ChmOrderCensus` builds 2,280 maps — five key shapes × nineteen sizes ×
six seeds × four constructors — and prints each one's iteration order as indices
into its insertion sequence, for `keySet`, `values` and `entrySet`. Diffed
against HotSpot JDK 25:

| | diverging maps |
|---|---|
| before | **1010** of 2280 |
| after | **376** of 2280 |

By constructor, the sized ones are nearly closed: `new ConcurrentHashMap<>(n)`
went 373 → 10. And **every remaining default-constructor divergence is a map of
12 entries or more** — that is, a map that has *resized*. Maps that never resize
are now exact, which is the case the H2 failure and the Spring alias case both
are.

## What is deliberately NOT fixed

**Maps that have resized.** CHM's `transfer` is not order-preserving the way
`HashMap`'s split is: it finds `lastRun` (the longest constant-destination
suffix of a bin), reuses that node directly, and **prepends** each node before
it onto the lo/hi list — so a resize reverses part of every bin. Reproducing
that needs the global insertion order of the surviving entries, i.e. a per-node
sequence number this implementation does not carry and which would cost an int
per entry across every CHM in the VM.

That residual is now *bounded and named* rather than unknown: buckets are
correct everywhere, and only the order within a bin, only after a resize, can
still differ. The census is the ratchet — it will report any regression in the
part that is fixed.

## The transferable part

**A number that is correct at construction is not therefore correct later.**
`chm_total_capacity` was exactly right when the Spring alias fix was written and
measured, and the map it was measured on never grew. The defect is not in that
reasoning; it is that nothing re-derived the number once the two capacities
could diverge.

**Use collision keys to isolate a tie-break.** The question "is our chain order
insertion order?" was unanswerable from the census, where the sort key and the
tie-break move together. Keys with identical `hashCode` make the sort a no-op
and leave the chain as the only variable — a one-run answer to a question that
had looked like it needed a per-node sequence number to settle.
