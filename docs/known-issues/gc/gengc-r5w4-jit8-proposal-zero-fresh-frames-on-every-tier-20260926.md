# Proposal: every compiled frame starts all-zero, on every tier, and it is a default

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 48
> of 54).** The IR half landed opt-in (`CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`,
> `Lowerer::emit_zero_whole_frame`); the single-pass spill-band lever is
> `CRATONVM_JIT_SP_ZERO_SPILL_BAND` (opt-in). Absorbs
> `../../internal/gc/gcd-d2f-proposal-single-pass-written-words-claim-REJECTED-20260928.md` as the
> zero-cost alternative (section at the end). No d7 probe needs either now
> (OsrDeadSlot fixed); ArgPin's remaining channel is
> `gcd-d4o-proposal-callee-owned-arguments-20260928.md`'s. **Gate:** one
> pricing run picks the design: `BinTreesClassic 18` and CratonBench
> `fib`/`hashmap`, `CRATONVM_JIT_SP_ZERO_SPILL_BAND` on vs off, interleaved. A
> cost under noise defaults the zeroing; above it, build the written-words
> claim. **Size:** S (pricing), M (either design).

> **STATUS (2026-09-26, gen r5w4/jit8): PROPOSAL.** The optimizing tier's half
> landed opt-in this wave (under `CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS=1`,
> `Lowerer::emit_zero_whole_frame` in `jit/src/ir_lower.rs`). This page is the
> rest, and the case for a default.

*Filed 2026-09-26 by gen round 5 wave 4, lane `jit8`.*

## The idea

The collector reads compiled frames conservatively in more places than any
liveness claim will ever cover (the bookkeeping tail, `Prim` colours, the
staging region, the blind GPR spill image, the single-pass tier's unclaimed
regions). Every one of those words is dangerous for exactly one reason: until
THIS activation writes it, it holds whatever the previous frame at the same
depth left there. After a program drops a large structure, that is often the
structure's address, left by the callee that built it at exactly this depth
(`GenR5W3OsrHolderProbe`'s `warm-build-warm-check`, the `println` chain after
`GenR4W4NativeStringOomProbe`'s OOME).

Rather than prove each region dead, make the question moot: a fresh frame is
all zeros, so an unwritten word is `0` and never a root. What remains are
words this activation wrote -- which the existing claims (dead `Ref` colours,
precise frame liveness, the register-oop mask) are about.

## What it takes

1. **Single-pass tier** (`jit/src/x64/frames.rs`, `emit_prologue`): the same
   R10/R11 downward loop after `emit_stack_bang_headroom()` and before the
   callee-saved saves. Check first that no single-pass entry convention passes
   anything in R10/R11 (the IR tier's does not).
2. **IR OSR entry stubs** (`Lowerer::emit_osr_entry_stubs`): the stub builds
   its own frame and jumps past the prologue, so zero `[rsp, rbp)` in the stub
   before it seeds the locals.
3. **aarch64** (`jit/src/aarch64_backend.rs`): `stp xzr, xzr` pairs, same
   placement.
4. **Cost, measured before any default:** one store per 8 frame bytes per
   call. For leaf-heavy code (small frames) that is a handful of stores; for
   the large IR frames it can be hundreds. Price on `GenR4W4EvacThroughputProbe`,
   CratonBench `fib`/`hashmap`, and the Spring Boot sample start-up, interleaved
   (`cratonvm-microbench-noise`: ~3x swings, take medians). A size cap (zero
   only frames up to N bytes, and above it only the regions no claim covers)
   is the obvious knob if the big frames cost.
5. **Default once priced:** it drops nothing a live program can read (a word
   no instruction of this activation has written is unreadable by Java
   semantics; the prologue already zeroes selected ones on that argument), so
   by this round's rule it may land default-on when it is cheap enough.

## Why this before more liveness claims

Each claim so far (`CRATONVM_GC_DEAD_SPILL_ROOTS`, the register-oop mask,
`CRATONVM_JIT_IR_CLEAR_DEAD_REF_SLOTS`, `CRATONVM_JIT_PRECISE_FRAME_LIVENESS`)
needed its own soundness argument and its own oracle, and each left a named
residue (`../../internal/gc/gengc-r5w2-oomjit6-compiled-frame-residue-residuals-RETIRED-20260928.md`).
Zeroing at entry removes the entire "leftover" class with one argument and no
scanner change, and it makes the remaining claims' residues smaller and
easier to read in the holder census.

## How to verify

With the tier(s) done and the flag on: `GenR4W4NativeStringOomProbe` prints
its four `--nojit` lines; `GenR5W3OsrHolderProbe`'s holder census names no
holder with `prov="method=... region=operand-spill|outgoing-args-or-deopt-regs|..."`
at a frame of a method other than the one running at the check. Unit tests:
the loop bytes per tier (as `the_fresh_frame_zeroing_loop_is_self_contained`)
and an execution test that calls a compiled method twice at the same depth
and asserts, from a debug hook, that the second frame's unwritten words read
zero.

## Merged from `gcd-d2f-proposal-single-pass-written-words-claim` (d8/y, 2026-09-28): the zero-cost alternative for the single-pass tier

Retired as a duplicate of this page. Instead of zeroing a fresh single-pass
frame, the compiler records per safepoint which operand-spill offsets some
path from entry may have written (a forward may-dataflow, union at merges,
never removing an offset) and publishes it as
`OopMapEntry::band_written_mask: Option<u64>` over `(off - spill_lo) / 8`
(`None` above 64 words or when unproven). The band scan
(`vm/src/jit/conservative_roots.rs`, beside `spill_slot_is_dead_above_cursor`)
then drops a spill word whose bit is clear. It is sound because it is a claim
about this activation's own stores, which the compiler emits; a missed store
only leaves a bit clear, so every writer must be counted
(`reserve_spill_slots`, the inline splice areas, helper-argument buffers,
`flush_scratch_registers`, `invalidate_callee_saved`, deopt/OSR stubs; an OSR
body starts full). It costs nothing at run time and one `u64` per map, and
keeps `CRATONVM_JIT_SP_ZERO_SPILL_BAND` as its oracle (under
`CRATONVM_DBG_VERIFY_REG_OOP_MAPS`, a word the claim drops must read zero when
the lever is also on). Verify: a `jit/src/x64/tests.rs` unit test (a second
safepoint after a splice); `GenR5W2OsrDeadSlotProbe` / `GenR5W3OsrHolderProbe`
as under the lever; `BinTreesClassic 18` unchanged in time.

Which one to build is the pricing run in the status block: zeroing if it is
under noise on call-heavy code, this claim if not.
