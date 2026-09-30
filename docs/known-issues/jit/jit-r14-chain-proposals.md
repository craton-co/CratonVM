# JIT round 14 wave 1, lane chain: proposals (the chain arm and IR frame words)

Status: OPEN (proposal book; ranked; nothing here is a defect)
Area: `jit/src/ir_lower.rs` (`plan_slots`, the frame block), `jit/src/lib.rs` (the IR inline planner), `jit/src/ir.rs` (chain snapshots)
Found by: round 14 wave 1 lane chain

Context: `r13w11-chain5-chain-arm-hashmap-kernel-slower-than-default-20260928.md` (round 14 wave 1
section) and `r14w1-chain-frame-block-home-cap-patch-FIXED-20260929.md`. Ranked by expected benefit over
cost.

## CHN-1. Range pins for the compiling method's own snapshots too (HIGH value, medium cost, medium risk)

**What.** Round 14 wave 1 stopped `plan_slots` pinning values only CHAIN snapshots name
(`CRATONVM_JIT_IR_CHAIN_SNAPSHOT_RANGE_PINS`): such a value keeps its word through every node that
could publish the snapshot, via `plan_slots`' own dataflow, and then shares it. Every value a FLAT
snapshot names is still `SlotClass::Pinned` for the whole method, and the builder records a flat
snapshot at every bytecode boundary (the operand stack included), so on real bytecode most
reference temporaries own a word for the whole activation.

**Why.** Frame size, GC roots kept alive for the whole activation (the reason the dead-home clears
exist), and the frame-block cap: every pinned reference is a home the per-call publication copies
twice per call once a body passes 32 (see the patch page). The default arm's `HashMap.putVal` is
30 KB of code; how much of that is per-GC-point publication is the first thing to measure
(`CRATONVM_DBG_IR_RELOC=1`).

**Cost / risk.** A flat snapshot has many readers, unlike a chain one: guards and implicit checks
at their resume bci, `build_deopt_points` at `bci_native` (the earliest native offset of the bci),
call-site points (return / exception / deferred callee deopt services), OSR entries (seed homes at
a header), mode exits and rethrow pads. Each must become a use at its native position before a
flat-named value can share. First step: a census under `CRATONVM_DBG_IR_SLOTS` of Pinned homes vs
`peak_live` per compile on the R13 battery and Spring, to size the win before touching readers.

## CHN-2. Publish the frame block out of line, and choose block vs per-call by loops (HIGH value, medium cost)

**What.** `emit_frame_block_ensure` emits the whole block publication (~14 bytes per home) inline
at EVERY GC point, which is why the block is capped at 32 homes; past it every GC point copies
every defined home to the shadow stack and back. Emit the publication once per body as a stub
(`JNE stub` at each GC point, the stub returning through a per-site slot or a `CALL`-shaped
local routine the frame walker already tolerates), then drop the cap, or keep a cap only for
bodies with no GC point inside a loop.

**Why.** A one-activation kernel (every OSR'd benchmark loop) with a call in its loop and more than
32 reference homes pays 2N memory operations per call today, in both arms.

**First step.** The knob and diagnostic of `r14w1-chain-frame-block-home-cap-patch-FIXED-20260929.md`,
then `CRATONVM_JIT_IR_FRAME_BLOCK_MAX_HOMES=128` timed against 32.

## CHN-3. Judge a top-level multi-return splice's cold callees by the compiling method's profile (MEDIUM value, low cost)

**What.** In the chain arm `HashMap.putVal`'s own compile splices `treeifyBin` (a TOP-level site
of putVal; the chain fences relax the `ir-splice-trap-after-call` refusal that keeps it a call in
the default arm, `hs2-D.log`). `ir_splice_prune_cold_nested` never judges a top-level site ("it
has a profile and the hot-loop tiers"), but nothing uses that profile either: the branch in front
of the call (`binCount >= TREEIFY_THRESHOLD - 1`) is never taken on these kernels.

**Why.** Code size and GC-point count in a body everyone calls (7 KB of `treeifyBin` code per
`putVal` copy, plus its calls' publication).

**First step.** In the planner, drop a top-level site larger than `MAX_INLINE_SIZE_COLD` whose
block the compiling method's branch profile marks never reached (`ir_branch_hints` already has the
counts), keeping the call row, under a default-on switch read only in the chain arm.

## CHN-4. Record chain snapshots only where something can trap (MEDIUM-LOW value, low cost)

**What.** `IrBuilder::push_splice_state` records a chain snapshot (locals + every scope's frame) at
every instruction of every spliced body; `ea_ir_bridge::ir_prune_unconsumable_snapshots` drops the
ones no deopting node consults only after escape analysis. Record them only at pcs whose opcode can
trap or that a fence relaxation asks about (`chain_state_at` needs the last snapshot at the pc).

**Why.** Compile time and memory (the chain arm's big compiles were lowered twice and are the
largest graphs of the kernel), and fewer values named at all before the prune (EA and the sink
pass read them).

**Risk.** `relax_fence_by_chain` requires a chain snapshot at exactly the fenced pc; the fence arms
are the list of pcs that must keep one.
