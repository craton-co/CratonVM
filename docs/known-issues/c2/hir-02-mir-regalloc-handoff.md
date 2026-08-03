# HIR-02 — give the instruction selector somewhere to send its output

**Status:** increment 0 **DONE 2026-08-03**; increments 1–4 **on hold, and
the hold is the finding.** **Depends on:** `hir-01`'s contract —
settled 2026-08-03 as `docs/jit/lowering-contract.md`. **Read §5 and §5.1 of
that before this file**: they reorder the increments below, and §5.1 is the
measurement that says not to start the next one yet.

> **Increment 0's answer.** Shadow selection (`CRATONVM_JIT=ir-isel-shadow`,
> default off, emits nothing) ran over 850 real Spring Boot compiles.
> `select_block` covers **15.7–19.0%** of scheduled data nodes — not the 38.2%
> the synthetic corpus predicted — and of the four rules that fire, one
> (`TestBranch`, 60 tiles) is byte-for-byte what `ir_lower` already emits.
> **`Rule::Lea` and `Rule::AluImm` fire ZERO times**, because the pattern table
> is 64-bit and Java arithmetic is 32-bit.
>
> So the first increment below should NOT be "give the selector somewhere to
> send its output". It should be `instruction-selection.md` §6 item 2 — the
> 32-bit rows — followed by re-running the same flag. Building a machine level
> to carry tiles that mostly say `Generic` is the migration cost without the
> migration's benefit.
>
> `covers()` held on all 1 911 blocks, which is the one piece of good news: the
> invariant the contract's §6.1 argument rests on is real on real code.
**Owns:** `jit/src/x64/isel.rs`, a new `jit/src/mir.rs` (or whatever `hir-01`
names it), `jit/src/regalloc.rs`.

## Current state, verified

* `jit/src/x64/isel.rs` (6 848 lines) has a real IR-level tiler: address-mode
  folding, compare-and-branch fusion, `LEA` for three-operand adds, immediate
  folding with checked width, and a load-folding gate that runs the alias
  oracle against **every** intervening node. It is tested and it has **no
  caller**. See `docs/jit/instruction-selection.md`.
* It had never been compiled at all until 2026-08-01 — there was no `mod isel;`
  in the crate. The first compile surfaced a real cost-model bug. It **is**
  compiled now (`x64.rs:136`) and its tests pass — 68 of them, re-run
  2026-08-03. What is still unverified by execution is the *output*: nothing
  emits a tile.
* `jit/src/regalloc.rs` has linear scan with `verify_allocation`, and one
  consumer that uses it as a write-through register *read cache* — because
  there is no form in which "this value lives in this register" can be
  expressed and then encoded.
* **A third component is in the same state for the same reason**, and this
  lane's census missed it: `jit/src/x64/vec_emit.rs::emit_vector_loop` has no
  caller either, and carries the private XMM pool hazard 2 below. Re-derive
  before assuming the file list is complete.

The gap between those two is the whole lane: the selector produces tiles, the
allocator produces assignments, and nothing consumes either.

**Measured, so the lane is not sized by intuition.** Over a ten-shape
integer/branch/loop corpus (probe in the contract's appendix), `select_block`
covers **38.2%** of scheduled data nodes with a real rule; `Rule::AluReg`
accounts for 14 of the 18 firing tiles; `Rule::AluImm` fires **zero** times
because the table has no 32-bit immediate rows. That is the ranking for the
first two increments, and it is also the reason the contract insists the
measurement be re-taken over real suite compiles before anything is emitted.

## The first increment

A minimal machine-level form that is **only** what the two existing components
already produce and consume — no more:

1. A tile list with virtual registers.
2. An allocation over it, verified by the existing `verify_allocation`.
3. An encoder that turns exactly the tiles `isel` can already emit into bytes.

Then prove equivalence rather than asserting it: for a corpus of methods,
compile through both paths and compare **emitted bytes**. `isel`'s pattern
table already has this property per row — every row names the hand-written
emitter it reproduces byte-for-byte — so the corpus comparison is the same
check at method scale.

## The two hazards that decide this lane

1. **Safepoints.** A reference that is register-resident at a safepoint and not
   published in the frame map is a root the collector cannot see. The oop map
   consumer takes frame slot offsets only. Either the new path keeps refs
   memory-pinned (what the write-through cache does today) or the map format
   and its consumer change together — and that consumer is in `vm/`, outside
   this lane's files. Decide before writing the encoder, not after.
   **Decided:** `docs/jit/lowering-contract.md` §3 — keep refs memory-pinned
   through increment 2, do not touch `OopMapEntry`. Note while reading that
   section that *deopt* already has a full register bank
   (`FrameValue::RegisterRef`, `SavedRegisters`) and *GC* does not; the
   asymmetry is deliberate, because the deopt stub spills the register file and
   a GC frame walk does not. Do not "fix" it by making them symmetric.
2. **The vector register pool.** The vector emitter carries a private XMM0–5
   pool because `regalloc.rs` has no vector class. Unifying them is listed as
   the single highest-risk prerequisite in `docs/jit/vectorization-emitter.md`:
   a scalar FP value living in those registers is silently destroyed across a
   vector region. If this lane touches `regalloc.rs`, it owns that unification.

## How to verify

Byte-for-byte equivalence on a corpus, plus `verify_allocation` on every
allocation, plus a bail-out on any mismatch. Off by default behind a declared
flag until the corpus is clean.

## What to refuse

Any tile the pattern table cannot encode. The table's one trustworthy property
is that every row is anchored to a byte literal the existing backend emits;
inventing unanchored rows destroys that and there is no test that would catch
it.
