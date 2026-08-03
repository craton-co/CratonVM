# HIR-01 — settle the lowering contract before splitting the IR

> **RETIRED 2026-08-03.** The contract this lane asked for is
> **`docs/jit/lowering-contract.md`**. Read that; what follows is the brief it
> answers, kept for the questions it posed and the standard it set.
>
> Summary of the answer, so nobody has to re-derive it:
>
> * **Four levels, not three** — bytecode, `ir::Graph`, a missing machine list,
>   and encoding. The report's "HIR" already exists and it is the bytecode.
> * **Level 2 is missing as an artifact, not as a design.** `isel`,
>   `allocate_linear_scan` and `vec_emit` are three finished components that all
>   dead-end on the same absence: no value in this compiler means "an
>   instruction whose operands are values, not addresses".
> * **The oop-map obligation lives with whoever assigns registers**, and the
>   rule — no reference register-resident at a GC safepoint — is already
>   discharged twice, differently, by the two production backends. Deopt has a
>   register bank; GC does not, and that asymmetry is load-bearing.
> * **The three-defect test scores one of three.** The monitor defect becomes
>   unrepresentable; the escape-analysis and dead-store defects are level-1
>   analysis bugs a lowering contract does not touch. So the migration is
>   **not** justified as a correctness investment — it is justified, or not, by
>   the coverage measurement the first increment produces.
> * **Increment 0 emits nothing** and is where the decision gets made. Measured
>   for the contract: `isel` covers 38.2% of scheduled nodes on an integer
>   corpus, and `Rule::AluImm` fires zero times because the table has no
>   32-bit immediate rows.
>
> Three stale claims in neighbouring docs were found and corrected while
> answering this; see §8 of the contract.

**Status:** RETIRED — answered by `docs/jit/lowering-contract.md`.
**Blocks:** `hir-02` (now unblocked). **Owns:** `docs/` only —
this lane writes a contract, not code.

## The claim to check first

The report asks for HIR/LIR/MIR. CratonVM has **one** IR level: `jit/src/ir.rs`
(sea-of-nodes, ~8k lines), lowered directly to machine code by
`jit/src/ir_lower.rs` (~10.6k lines). There is no intermediate form.

Before proposing three levels, establish what the single level is actually
failing at. That is a real question with a real answer in this tree, and the
answer may be "two levels, not three". Evidence to weigh:

* `ir_lower` is simultaneously doing instruction selection, register
  assignment, frame layout, safepoint publication and encoding. The wave found
  that its value model has no register-residency concept at all — a linear-scan
  consumer had to be added as a *write-through cache* rather than a real
  allocator precisely because the lowerer cannot express "this value is in a
  register" (`docs/jit/linear-scan-wiring.md`).
* `jit/src/x64/isel.rs` exists, is now compiled, and has an IR-level tiler with
  a cost model — but no production call site, because its output has nowhere to
  go. That is the missing level, concretely.
* The single level is why `ir_lower`'s catch-all was able to compile a monitor
  op to *nothing*. A typed lower-level form makes "no rule matched" a
  representable, refusable state instead of a silent `_ => {}`.

## The deliverable

One document that answers, with file evidence, not opinion:

1. **How many levels, and what does each one own?** Name the property each
   level is allowed to assume. A level whose invariants you cannot state is a
   level you cannot verify.
2. **Where does the safepoint/oop-map obligation live?** Today
   `emit_safepoint_map` publishes *frame slots only*, and there is no register
   bank in the oop map — which is why references are pinned to memory. Any
   multi-level design has to say which level owns that obligation, because it
   is the difference between a leak and a use-after-free.
3. **What is the migration path that keeps the tree green at every step?**
   A big-bang IR rewrite in this repo is not reviewable. The linear-scan lane's
   shape — new path behind a declared flag, cross-checked against the existing
   path, verifying its own output, bailing out on mismatch — is the pattern
   that landed. Say whether it generalises here.
4. **What does the first increment produce that is independently useful?**
   If the answer is "nothing until all three levels exist", the design is
   wrong for this codebase.

## How to verify the contract is sound

Take three defects this campaign found and ask whether the proposed contract
would have made each one **unrepresentable** rather than merely less likely:

* the monitor op that lowered to nothing;
* the escape-analysis pass that forwarded a load to a *later* store;
* the dead-store elimination whose location key could not distinguish two
  `int` fields of the same object.

A design that only makes them "less likely" is not worth the migration cost.

## What to refuse

Do not open with a crate skeleton, a trait hierarchy, or a `mir/` directory.
The output of this lane is prose plus a migration plan. If it produces code,
it produced the wrong thing.
