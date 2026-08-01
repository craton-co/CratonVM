# HIR-02 — give the instruction selector somewhere to send its output

**Status:** not started. **Depends on:** `hir-01`'s contract.
**Owns:** `jit/src/x64/isel.rs`, a new `jit/src/mir.rs` (or whatever `hir-01`
names it), `jit/src/regalloc.rs`.

## Current state, verified

* `jit/src/x64/isel.rs` (~6.8k lines) has a real IR-level tiler: address-mode
  folding, compare-and-branch fusion, `LEA` for three-operand adds, immediate
  folding with checked width, and a load-folding gate that runs the alias
  oracle against **every** intervening node. It is tested and it has **no
  caller**. See `docs/jit/instruction-selection.md`.
* It had never been compiled at all until 2026-08-01 — there was no `mod isel;`
  in the crate. The first compile surfaced a real cost-model bug. Treat every
  remaining claim in that file as unverified by execution.
* `jit/src/regalloc.rs` has linear scan with `verify_allocation`, and one
  consumer that uses it as a write-through register *read cache* — because
  there is no form in which "this value lives in this register" can be
  expressed and then encoded.

The gap between those two is the whole lane: the selector produces tiles, the
allocator produces assignments, and nothing consumes either.

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
