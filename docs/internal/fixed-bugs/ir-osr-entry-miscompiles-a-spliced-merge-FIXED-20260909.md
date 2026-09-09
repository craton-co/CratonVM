# The optimizing OSR entry miscompiles a body with a spliced merge

**Status:** ✅ FIXED 2026-09-09, same day it was opened. The containment —
`ir::SPLICED_MERGE_SEEN` withholding `ir_osr_entries` — is removed with the
fix. **Retired from** `docs/known-issues/`.
**Reproducers:** `docs/internal/repros/ir-osr-spliced-merge-20260909/`
(`Min0.java`, `Min1.java`, `Min2.java`). Unit-level, in `jit/src/ir_lower.rs`:
`an_osr_entry_seeds_the_constants_its_merge_arms_read` and
`a_spliced_branchy_body_runs_from_both_doors`.

The page's own localisation was **right about where and wrong about what**. It
is kept below, unedited, because being wrong in a specific enough way to be
checked is what made the next step cheap — and because the correction is the
useful part of the record.

---

## What it actually was

`emit_osr_entry_stubs`, as the page said. Not a phi home, and **not the
splice**.

`Min0.java` is the correction: the same three-armed clamp written straight
into the loop body — no call, nothing spliced, no combined-buffer pc anywhere
in the graph — reproduces it exactly.

```
expected (HotSpot)   clamp=5134452788
run 1                clamp=-3850602973011351
run 2                clamp=7354329854667288
run 3                clamp=341243207978836
```

So the population was never "methods with an inlined branchy callee". It is
**any in-loop merge one of whose arms is a literal**, which is
`if (v < 100) r = 100;` and every `?:` in the language. `ir-splice-branch` made
it reachable through one more shape; it did not make it. The page's own arm
table holds unchanged on `Min0`, including the decisive one:
`CRATONVM_JIT_IR_OSR_ENTRY=0` is correct, everything else is still wrong — on
a program that splices nothing.

### The word being read

`CRATONVM_DBG_JIT_DISASM=Min0.runClamp` prints both entries into one artifact.
The entry block materialises each constant into its own frame slot:

```
140: mov qword [rbp-0A8h],41C64E6Dh   ; 1103515245
161: mov qword [rbp-0D8h],64h         ; 100   <- the low arm
16c: mov qword [rbp-0E0h],384h        ; 900   <- the high arm
```

and the merge stores read them back from there:

```
230: mov rax,[rbp-0D8h]      ; r = 100
237: mov [rbp-0E8h],rax      ;   ... into the phi's home
257: mov rax,[rbp-0E0h]      ; r = 900
25e: mov [rbp-0E8h],rax
```

The entry stub builds its own frame, zeroes the JVM-local homes, seeds the
locals the snapshot names — and **jumps straight to the loop header**, past the
block those `mov qword` stores live in. `[rbp-0D8h]` therefore holds whatever
the previous frame at that address left behind, and `r = 100` reads it. That is
the "frame word nothing wrote" the page correctly inferred from the varying
answer; it belongs to a CONSTANT, not to a phi.

`1103515245` is fine on the same path and `100` is not, which is the detail
that makes it legible: `imul eax,41C64E6Dh` folds that constant into an
IMMEDIATE (`ir_const_imm_enabled`), so that reader never touches a home word. A
phi input gets no such treatment — `emit_phi_copies` and the merge stores read
a value from its home word, always.

### The check that let it through

`emit_osr_entry_stubs` refuses a bci unless every value live on entry is one
the snapshot names. Constants were exempt, on stated grounds:

> A CONSTANT can [produce itself]: every reader materialises it as an immediate
> (`ir_const_imm_enabled`), so its range crossing the header says nothing about
> what has to be seeded. Counting it as unseedable refused every loop in the
> language.

The second sentence is true and the first is not. Exempting a constant is the
right call — a constant is exactly the kind of value an entry can produce for
itself. Exempting it *without emitting it* was the defect, and the two halves
had simply never been written down together.

## The fix

The stub re-materialises, in itself, every `Op::Const`/`Op::ConstF` **live
across** the entry position: the home word, plus the register copy
`publish_fp_from_slot` makes for a float or double — byte-for-byte what the
node's own definition emits, because that is what the skipped block would have
done.

Live-across, and not "every constant with a slot", for a reason worth stating:
spill slots are COLOURED (`slot_plan.node_color`), so a constant that is *not*
live at the entry may share its word with a value that is, and writing it would
clobber a seed. Live-across is exactly the set whose colour is exclusive at
that point — the same predicate the eligibility test uses, with the two halves
now consistent.

`ir::SPLICED_MERGE_SEEN` and the `cm.ir_osr_entries` withholding it drove are
gone.

## Why the containment could come off rather than stay as belt-and-braces

It was never load-bearing for anything but this defect, and it cost the
population it covered: a method whose loop body inlines a branchy leaf got no
optimizing OSR entry at all, which is the entry the IR tier exists to serve on
a long-running loop. Keeping a refusal after its cause is fixed is how a tier
quietly loses coverage nobody can later account for.

`a_spliced_branchy_body_runs_from_both_doors` asserts the loop header **has**
an optimizing entry rather than tolerating its absence, so the containment
cannot come back silently and leave that test passing by never entering.

## What was verified

Binary built from this branch on the Azure box, against Temurin JDK 25 on the
same machine.

| | HotSpot | CratonVM |
|---|---|---|
| `Min0 8 800000` | `clamp=5134452788` | same, three runs, identical |
| `Min1 8 800000` | `clamp=[5134670375] norm=[53693838377357]` | same, three runs, identical |
| `Min2 400000 8` | `body=[2567207108]` | same |

`Min1` also matches with `CRATONVM_JIT_IR_SPLICE_BRANCH=0` and with
`CRATONVM_JIT_IR_OSR_ENTRY=0` — the two arms that used to be the difference
between right and wrong.

**And the door is actually taken**, which is the claim a passing checksum on
its own would not support. With the containment gone, `Min1.runClamp` — whose
`clamp` really is spliced (`[ir] spliced 1 callee body into
Min1.runClamp(II)J`) — publishes an entry and is entered through it:

```
[cratonvm-jitc] osr optimizing Min1.runClamp pc=8: stub=true entries=[8] sentinel_free=true
[cratonvm-jitc] osr optimizing REUSE Min1.runClamp pc=8
```

Under the containment that line read `entries=[]`. The refusal census for the
same compile is `("no safepoint snapshot at this block start", 5)` — the five
blocks INSIDE the relocated body, still correctly refused, because a
combined-buffer bci has no snapshot and is not a bci the interpreter can be
standing on. Only the loop header is offered, which is the whole point.

* `regression-suite/run.sh` — 92 passed, 0 failed.
* `cargo test -p cratonvm-jit -p cratonvm-types` and
  `cargo test -p cratonvm-vm --lib` — 30 targets green, 2643 vm tests passed,
  0 failed.
* Both unit tests are mutation-checked against the fix removed: they return
  `4176093576` where `6400` is the answer, and `4263735832` where `1300` is.
  Neither passes vacuously.

## What the lifted containment is worth — measured, and thin

The containment cost `Min1.runClamp` its optimizing OSR entry, so the honest
question on the way out is what that entry buys. `Min1 8 20000000` (160 M
clamp calls), ten interleaved pairs of the optimizing door against
`CRATONVM_JIT_OSR_OPTIMIZING=0`:

| | median | pairs won |
|---|---|---|
| optimizing OSR entry | 2.33 s | 8 of 10 |
| single-pass OSR | 2.64 s | 2 of 10 |

About 12% and 8 pairs of 10 — **suggestive, not established**. The
within-arm spread is 1.44 s to 3.63 s on a box that was running other agents'
builds throughout, which is larger than the difference between the arms; a
sign test over ten pairs at 8–2 does not reach significance on its own. Recorded
so the next person does not re-derive the same non-answer, and so that "the
containment cost nothing measurable" is not quietly assumed in either
direction.

What IS established is that the entry is correct and available again, which is
the property the containment traded away.

---

# The page as it was opened

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
