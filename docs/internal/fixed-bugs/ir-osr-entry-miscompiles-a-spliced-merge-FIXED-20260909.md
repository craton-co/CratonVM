# The optimizing OSR entry miscompiles a body with a spliced merge

**Status:** open. Contained, not fixed — such an artifact now publishes no
`ir_osr_entries`, so the OSR door falls back to the single-pass body. Found
2026-09-09 while enabling IR-tier inlining of callees that contain branches.

## Symptom

Wrong, and **run-to-run varying**, results. The same binary on the same
classes:

```
expected (HotSpot)   clamp=5134670375
run 1                clamp=57869429018787
run 2                clamp=381904177424547
run 3                clamp=419083683579555
```

A varying wrong answer from a deterministic program is the signature of reading
a frame word nothing wrote.

## Reproducer

`Min1.java` — a three-armed `clamp` funnelling to a single return, called from a
counted loop so the caller is entered through OSR rather than by invocation
count:

```java
static int clamp(int x, int lo, int hi) {
    int r;
    if (x < lo) r = lo; else if (x > hi) r = hi; else r = x;
    return r;                       // one return, two internal branches
}
static long runClamp(int n, int seed) {
    long s = 0; int x = seed;
    for (int i = 0; i < n; i++) { x = x*1103515245+12345; s += clamp(x>>>20, 100, 900); }
    return s;
}
```

`cratonvm -cp . Min1 8 800000`, with `CRATONVM_JIT_IR_SPLICE_BRANCH=1`
(the default since 2026-09-09).

## What it is NOT

Each of these was measured, not assumed:

| arm | result |
|---|---|
| `CRATONVM_JIT_IR_SPLICE_BRANCH=0` | correct — no spliced branch, no merge |
| `CRATONVM_JIT_IR_OSR_ENTRY=0` | **correct** — the splice still happens |
| `CRATONVM_JIT_OSR_OPTIMIZING=0` | correct — the door is not taken |
| `CRATONVM_C2_ACCEPT=never` | correct |
| `CRATONVM_JIT_OSR_OPTIMIZING_CACHE=0` | still wrong — not the artifact cache |
| `CRATONVM_JIT_IR_PHI_RESIDENCY=0` | still wrong |
| `CRATONVM_JIT_IR_DROP_PHI_HOME=0` | still wrong |
| `CRATONVM_JIT_IR_DROP_HOME=0` | still wrong |
| `CRATONVM_JIT_IR_PHI_COPY_REGS=0` | still wrong |
| `CRATONVM_JIT_LICM=0`, `CRATONVM_JIT_UNROLL=0`, `CRATONVM_JIT_IR_LINEAR_SCAN=0` | still wrong |

`CRATONVM_JIT_IR_OSR_ENTRY=0` restoring correctness **with the splice still
enabled** is what localises this to the OSR entry stub.

The built graph was read node by node and is correct. The merge of `clamp`'s
three arms is `Merge <- [36, 40, 39]` with `Phi <- [12, 31, 32, 30]`: the
not-taken edge of `x >= lo` supplies `lo`, the not-taken edge of `x <= hi`
supplies `hi`, the taken edge supplies `x`. The loop's own phis (`s`, `x`, `i`)
are seeded and back-edged correctly.

And the **same body through the method-entry door is correct**: `Min2.java`
calls the same `clamp` from a caller invoked 400 000 times with an eight-trip
inner loop, so it tiers up on invocation count rather than by OSR, and matches
HotSpot exactly.

## Where to look

`ir_lower::emit_osr_entry_stubs`. An optimizing OSR entry builds this tier's
frame and seeds the locals itself — unlike the single-pass door, which has a
fixed local→home map and a trampoline outside the body. The new thing in these
graphs is a `Merge`/`Phi` pair whose `bytecode_pc` is a COMBINED-BUFFER pc
(past `code_len`), living inside the loop body rather than at its header. The
first thing to check is whether the entry stub's frame preparation covers phi
home slots it does not seed — a phi written only on the edges into its merge
reads whatever the entry left in its word if the entry path reaches a use
first.

## The containment

`ir::SPLICED_MERGE_SEEN` is set when `IrBuilder`'s walk activates a merge while
a splice is open, and `lower_inner` then publishes an empty `ir_osr_entries`.
The OSR door checks `ir_osr_entry_addr(pc)`, finds nothing, and enters the
single-pass artifact.

This withholds only *entry at a loop header* for such bodies. Methods that tier
up by invocation count keep the inlining, which is the population it was built
for. Lifting the containment means fixing the entry stub, not disabling the
splice.
