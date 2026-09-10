# Two backends, one decision

**Status:** decided 2026-09-09. The IR tier stays, as an *analysis front end*
for the existing x64 machine backend. It does not stay as a second, independent
code generator.

This document exists because the question had never been asked in one place.
Every partial answer to it in the tree is a veto — a hand-maintained list of
methods the optimizing tier must not be allowed to take — and the vetoes were
accumulating faster than the tier was earning them.

## What was measured

On this branch, one host, one binary pair, arms interleaved with the order
flipped, medians of five, checksums matched against Temurin JDK 25 on every arm:

| workload | default | `CRATONVM_C2_ACCEPT=never` |
|---|---|---|
| call-dense kernel, 2 000 reps | 529 ms | 508 ms |
| pure counted loop, 20 000 reps | 262 ms | 204 ms |

Turning the optimizing tier off was faster in both. With compile cost amortized
over a long-running loop (10 × 20 M iterations) the two bodies were
indistinguishable — 649/654/668 ms with the C2 body against 643/667/668 ms with
the C1 body — while HotSpot ran the same loop in 293/299/334 ms.

So: the optimizing body is not better, and the tier's measurable effect was the
compile time it added. (That specific cost was the uncached OSR artifact, fixed
separately; the *neutrality* is what this document is about.)

## Why the optimizing body is not better

The two backends are not "a fast one and a good one". They are two optimizing
compilers, and the baseline is the better **code generator**:

| | single-pass (`x64/`) | IR tier (`ir_lower`) |
|---|---|---|
| Java locals | graph-coloured into callee-saved GPRs, references included | — |
| SSA values | compile-time operand-stack simulation, materialised on use | every value homed to a frame slot, store at every definition |
| register file | 7 GPRs on Windows, 5 on System V, authoritative | 5 GPRs as a **write-through read cache** over the frame |
| references in registers | yes, with a pre-safepoint spill | never — `OopMapEntry` has no register bank |
| LICM / BCE / null-check elim | yes | yes |
| escape analysis + scalar replacement | yes | yes |
| SIMD / SuperWord / unswitching | yes | no |
| `java/lang/String` access intrinsics | yes | no |
| precise exception frames (RBC.6) | yes | no |

Measured on one counted loop whose real work is about 16 instructions: the
single-pass body is 92 instructions with 47 register-to-register moves; the IR
body is 77 instructions with 29 frame memory operations and five jumps. The IR
tier's *analyses* are doing real work — it emits fewer instructions — and its
*emitter* gives the win back.

The veto lists are the evidence that this was already known, one method at a
time: `x64::single_pass_only_lowering_for` (the bulk-byte loop lowerings), the
`java/lang/String` intrinsic pin, and the PERF-01 sieve veto each exist because
somebody measured C1 emitting better code than C2 for a shape and wrote down an
exception. A tier that needs a growing exception list is not winning.

## The decision

**Keep the IR graph. Retire the second emitter.**

The IR tier's front half — sea-of-nodes construction, GVN, constant folding,
algebraic simplification, dead-store elimination, LICM over SCEV, full
unrolling, escape analysis, the scheduler and its frequency-driven layout — is
the part with no counterpart in the baseline and no argument against it. The
back half — `ir_lower`'s frame-homed value model and its own instruction
encoders — duplicates a backend that is better and cannot catch up without
re-implementing the vectorizer and the intrinsics too.

Retiring the second *emitter* rather than the tier means the target shape is:

```
bytecode → IrBuilder → ir_optimize → ir_schedule ─┐
                                                  ├→ x64 emitter + graph-colouring allocator
                          bytecode walk ──────────┘
```

with the x64 backend gaining an entry point that consumes a scheduled graph
instead of a bytecode walk. That is a real project, and the ordering below is
what makes it approachable rather than a rewrite.

## Ordering, and what each step buys on its own

Each of these is independently valuable, so none of them is a bet on the
end state landing.

1. **Register-describing metadata.** Give `OopMapEntry` a register bank and let
   deopt frame values name registers (`FrameValue::Register`, `RegisterLong`,
   `RegisterRef` already exist and the resume path already reads them). This
   was written down as the single blocker keeping *references* out of registers
   in the IR tier, and as the thing forcing the conservative whole-frame
   re-sweep in the baseline.

   **It is not the IR tier's blocker, and the IR tier's blocker is not
   metadata.** Both halves of that were tested on 2026-09-09 and both were
   wrong; what replaced them is below.

   **Note 1 (2026-09-09):** this was assumed to block `int`/`long` too. It does
   not. What blocked those was `deopt_nameable` — a whole-method
   register-exclusivity proxy standing in for the per-value question "can a
   deopt that can actually happen name this?". `compute_deopt_named_reachable`
   answers that question directly, and
   `extend_home_drops_to_carried_values` applies it to the single-use
   intermediates the residency planner skips on purpose. Together they remove
   dead frame stores worth ~3.4 % on a counted-loop kernel
   (`CRATONVM_JIT_IR_REG_AUTHORITATIVE=1`, 11 of 12 paired rounds).

   **Note 2 (2026-09-09): references do not need the oop-map work either, and
   promoting them does not pay.** Two separate findings, both measured.

   *The metadata was never missing.* This backend's register file is a
   write-through read **cache** over a frame slot, not an authoritative
   location. The slot is written at every definition — `value_home_droppable`
   and `phi_home_droppable` refuse `Ref` and still do — and
   `emit_safepoint_map` publishes that slot. So the collector's view of a
   promoted reference was always complete: it can find it and it can relocate
   it. The only thing a collection invalidates is the *copy*, and the fix for
   that is not to describe the register but to stop reading it —
   `ir_lower::invalidate_ref_residency` drops every cached reference at each
   point control can leave the body and come back. `OopMapEntry` is untouched,
   and `CRATONVM_JIT_IR_REF_RESIDENCY=1` is the whole feature.

   *What actually blocks it is the slow-path rejoin.* Every `getfield` and
   `putfield` this backend emits is a guarded inline read with the checked
   helper as its slow path, and that slow path **returns into the body** — on
   G1 and ZGC, which publish no read bounds for reference loads, it is taken on
   100 % of reference accesses (`emit_inline_compact_getfield`). A cached
   reference is therefore invalid again immediately after nearly every
   instruction that would have used it. The census says exactly that: on
   `bintrees`, `admitted=6 copies_dropped=6` — six references promoted, six
   copies thrown away.

   So the promotion costs a reserved register and a prologue save and avoids no
   reload. Measured on `bintrees`, interleaved and order-flipped, medians of
   ten: **+1.0 % with the range allowed to cross a safepoint (faster in 1 of 10
   rounds), +0.8 % restricted to safepoint-free ranges (2 of 10)**. Both
   directions lose, so the flag stays default-off and is kept for the
   measurement rather than for the speed.

   The consequence for this document is that reference promotion is not a
   metadata project. It is downstream of giving field access a path that does
   not call — which is the same work as step (4), and is where the ordering
   below now points.
2. **Frame state only where a deopt can arrive** — calls, allocations, guards,
   back-edge polls — instead of at every bytecode index. Removes the store
   pinning that `release_deopt_pins` currently has to undo, and cuts the deopt
   metadata that today runs at 8× the size of the code it describes.
3. **Authoritative allocation in `ir_lower`**, once (1) and (2) remove the
   reasons it cannot be. If this closes most of the gap on its own, the emitter
   merger becomes optional rather than necessary — and that is a legitimate
   outcome of this decision, not a reversal of it.
4. **Wire `x64::isel`.** The declarative pattern table already exists and is
   byte-for-byte verified against the hand-written emitters; nothing calls it.
   It is the natural seam at which one machine backend can serve two front ends.

## What this decision is not

It is not a statement that the IR tier was a mistake. Its analyses are the only
place in the tree where the classic optimizations are expressed over a real SSA
graph, and every one of them is a prerequisite for speculation — which neither
backend does today, and which is where the remaining 2× against HotSpot lives.

It is also not a licence to add vetoes. A new
`single_pass_only_lowering_for` entry is now a signal that step (3) has not
happened yet, and belongs in this document rather than in that function.
