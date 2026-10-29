# The "spill/frame-slot" class is mostly the accumulator lowering, not a missing register (rx27 lane regalloc)

**Status: OPEN.** Area: JIT optimizing tier, lowering and residency (`jit/src/ir_lower.rs`). Severity: performance.
Parent: `rx27-w4-per-row-instruction-breakdown-20261028.md` (lever 1, 4.5-9.9k instr/op per row).

## What the class counts, and what a register would change

The profile classes every `mov reg,[rbp-x]`, every `mov [rbp-x],reg` and every compare on a frame slot as
frame-slot traffic. The IR lowering is accumulator-shaped:

* each value is computed in RAX;
* it is then written to its home through `store_rax`;
* a resident value is also published to its register, and its home is kept written (write-through) by
  `install_caller_saved_residency` and by every value a frame state can name.

So the usual effect of giving a value a register is to swap one instruction for another:

* the home load becomes a register move (`mov rax,[x]` -> `mov eax,r8d`);
* a compare on the frame slot becomes a compare on the register (`cmp dword [x],k` -> `cmp r8d,k`);
* the definition becomes a publish (`mov r8,rax`), and the home store is kept anyway.

The instruction count, which is what `icount2` measures, does not move. That is what rounds 25 W8-4 and W9-2
measured (`CRATONVM_JIT_IR_REG_ALLOC`: neutral).

The count drops only where an instruction disappears:

1. **A value that is never materialised.** Instruction selection folds it into its single consumer. This round's
   change is the first of these (below).
2. **A home store that is dropped, not converted.** This needs a register AND no frame state that can reach it.
   Caller-saved values are write-through by contract, so this lever is open: `install_caller_saved_residency`
   clears `home_dropped` for every caller-saved value. A whole-range caller-saved value that is not republished
   and is not `deopt_named_reachable` could keep its drop, but the drop must be reported to the lowering ladder
   (`image_dropped`) so that a misprediction is retried.
3. **A value whose range crosses a block.** The `Start.match` excerpt (`mov [rbp-98h],rax; cmp dword
   [rbp-98h],80h; mov rax,[rbp-98h]`) is `ch` with one use in the `If` and one in its successor. The caller-saved
   file (R8-R11) is block-local by construction: `caller_saved_transparent_positions` marks no edge position
   transparent. So such values compete for the five callee-saved registers, against the loop's carried values.

   An extended-basic-block rule would admit them:
   * the edge `b -> b+1`, where `b+1` has `b` as its only predecessor and its head is a `Proj` of `b`'s
     back-edge-free `If`, is transparent;
   * `lower_block` keeps the residency across that edge instead of invalidating it.

   This rule only pays together with item 2.

## Changed this round: `CRATONVM_JIT_IR_AND_ZERO_TEST_BRANCH` (opt-in)

What it does: an `If` whose fused condition is `Cmp(Eq | Ne, And(x, mask), 0)` branches on `TEST x, mask`, and the
`And` emits nothing.

When it applies, all of the following hold:

* the `And` has a single use and no snapshot names it;
* nothing is carried into or out of it, and it has no register;
* the mask is an immediate;
* `x` is not overwritten before the branch.

Where it lives: `Lowerer::plan_and_zero_tests`, `emit_and_zero_test`, `test_reg_imm_bytes`,
`test_frame_imm_bytes`.

What it removes: per test, `mov rax,[x]; and eax,mask; mov [w],rax; cmp dword [w],0` becomes `test [x],mask`, or
`test reg,mask` when `x` is resident. That is three instructions and a store-to-load round trip. The regex
predicates (`ASCII.isType`, `CharPredicates`, `(ch & 0xFFFFFF80) == 0`) do this once or more per character.

Not covered:

* A test that the builder pruned into a `Guard` is not taken: the guard's frame names the `And`, the operand
  `ifeq` re-reads.
* A test whose `And` a snapshot names is not taken either.

The census separates those cases.

Census:
`[cratonvm-jitc] ir-and-zero-test <method>: and-zero branches=N fused=F refused: used-or-named= carried-or-resident= clobbered= shape=`.

Tests (`jit/src/ir_lower/and_zero_test_tests.rs`, executed):

* `rx27w4_an_and_zero_branch_is_one_test`: with the switch, both masks of a loop are `TEST`s and the body is
  shorter; without it there is no `TEST`; the answers equal the reference.
* `rx27w4_test_encodings`.

## What would close the class

Items 2 and 3 above, measured by `icount2`, not by the profile's class share. Take the census first:

* `CRATONVM_DBG_IR_LINEAR_SCAN=1` (`[ir-ls] skipped:` and `homes kept:`);
* `CRATONVM_DBG_JITC=1` (`ir-home-why`, `ir-ls-gp`).

Run them on `Pattern$Start.match`. They tell how many of its values are refused for a register (pressure) and how
many keep a home for a frame state.

## rx27 wave 5 (lane regalloc, 2026-10-28; pending build): levers 1 and 2, `CRATONVM_JIT_IR_LS_CALLER_SAVED_EBB` (opt-in)

One switch covers both levers, and it needs `CRATONVM_JIT_IR_LS_CALLER_SAVED` (default ON).

**Lever 2: residency across a single-predecessor edge.** The edge `b -> b+1` is transparent to the caller-saved
planner (`caller_saved_ebb_edge`, used by `caller_saved_transparent_positions`) when all of these hold:

* `b` ends in an `If` with no back edge;
* `b+1` is one of its successors;
* `b+1` has `b` as its only predecessor and is headed by a projection of that `If`.

`lower_block` then skips the block-end and block-start invalidation on that edge. The edge is not carried when:

* a fused `instanceof` jumped to the edges from inside `b`;
* `b+1` is a call-site exception entry.

After the body, `verify_caller_saved_ebb_carries` refuses the attempt if any branch reaches a carried block from
outside its predecessor's terminator code.

**Lever 1: a dropped home stays dropped.** `install_caller_saved_residency` no longer forces the slot store back
on for a caller-saved value when `caller_saved_home_may_stay_dropped` holds. That requires all of:

* the value is a whole-range value of the plan (`RegResidency::caller_saved_whole`), not split, and not
  republished after an opaque definition or anchor;
* `value_home_droppable` already dropped its home;
* `deopt_named_reachable` is false. That is the per-snapshot proof: no snapshot at a deopt or GC bci, no loop
  header and no loop-carried or promoted recipe names it;
* it is `int` / `long`, so no GC reader exists.

Every miss is a refused attempt, never a wrong read:

* an emitted read of the dropped slot hits `slot_of`;
* a frame state naming it hits `frame_value_for`.

The attempt is then lowered again with every home kept and no carry. `image_dropped` is set whenever the switch is
active, and the retry turns the switch off.

**Census:** `[cratonvm-jitc] ir-ls-caller-saved-ebb <method>: caller-saved values= whole-range= homes dropped=
edges carried=`.

**Tests** (`jit/src/ir_lower/caller_saved_ebb_tests.rs`):

* `rx27w5_the_single_predecessor_edges_of_the_loop`: the CFG rule on a real loop;
* `rx27w5_the_edge_loop_answers_alike_with_no_more_slot_traffic`: executed in both arms with answers equal to
  the reference. It counts the frame-slot `mov`s, asserts on <= off and prints both counts.

## rx27 wave 6 (lane regalloc, 2026-10-28; pending build): `CRATONVM_JIT_IR_AND_ZERO_TEST_GUARD` (opt-in)

Why: on cvm-rx27b7 the `If` rule fused nothing on `matchonly` (`used-or-named`): the masked test there is a
pruned branch's `Guard`, whose frame keeps `ch & mask` on the operand stack for the re-executed `ifeq`.

What it does: a `Guard` on `(x & mask) == 0` / `!= 0` deopts on `TEST x, mask`. When the guard's own frame names
the `And`, the guard's deopt pad rebuilds the `And`'s home before the stub reads the frame:
`mov rax, x; and eax/rax, mask; mov [home], rax`. `x` is read from where the `TEST` read it (register, home, or
RAX), and the pad is entered straight from the `Jcc`. This is rematerialisation, not a retry. The passing path
has no `AND` and no store.

Conditions (`Lowerer::plan_and_zero_guard_tests`):

* every snapshot naming the `And` is at the guard's bci, and no other node there can deopt;
* no recipe names it, and no soak exit reuses the point;
* the `And` has a planned home, a single use, no carry and no register, and `x` is not overwritten before the
  guard.

An `And` that no frame names is folded with no remat. A frame-state mismatch at emission is a refused attempt,
retried with every home kept (`image_dropped`; the plan is off on the retry).

Census: `[cratonvm-jitc] ir-and-zero-test-guard <method>: and-zero guards= fused= fused-with-pad-remat= refused: ...`.

Tests (`jit/src/ir_lower/and_zero_guard_tests.rs`), executed through a real deopt:

* `rx27w6_a_pruned_guard_names_its_and_and_the_pad_rebuilds_it`: the `TEST` is present; for nine inputs both arms
  give the same result or the same reconstructed frame (`x` in local 0, `x & -128` on the stack);
* `rx27w6_an_and_named_by_another_frame_keeps_its_and`.

## Measured on cvm-rx27b8 (int8 1864d114c0, prof lane, 2026-10-28): the arms are flat

icount2 main/op, median of 5. `rall` = `AND_ZERO_TEST_BRANCH` + `LS_CALLER_SAVED_EBB`.

| row | def | azt | ebb | rall |
|---|---|---|---|---|
| matchonly | 26012 | 25951 | 25832 | 25832 |
| regex-find-ci | 24559 | 24547 | 24559 | 24547 |
| regex-find-email | 36297 | 36969 | 36760 | 35934 |
| split-ws | 44374 | 44852 | 44653 | 44880 |
| cached | 40117 | 39200 | 40059 | 39403 |

Every arm is within noise:

* `matchonly` is bimodal on the same binary, so its numbers do not separate the arms;
* `split-ws` and `regex-groups` spread 9-15 % between repeats;
* the one consistent move is `azt` on `regex-find-ci`, -12 instr/op.

Facts equal HotSpot in both modes on every arm.

Why the arms do not move, from the b8 census:

* On the hot `matchonly` / `split-ws` loops the If fold was refused (`used-or-named`). The fix is 88acaad9d4, which
  is not in b8.
* The guard fold (`AND_ZERO_TEST_GUARD`) was not one of the b8 arms.
* `LS_CALLER_SAVED_EBB` carries many edges, but it keeps 1-2 values per hot method in a register and drops their
  stores. That is too few to show in the instruction count.

Next: b9 (int10 045ee6dc0e, with 88acaad9d4), arms azt / azg / rall2. Every switch stays opt-in.

## rx27 wave 7 (lane regalloc, 2026-10-28): the slot census

A wave-7 switch, `CRATONVM_JIT_IR_HOME_STORE_ELIDE`, was removed on 2026-10-29. It dropped an RAX reload right
behind its own store and skipped the stores of values nothing read. On cvm-rx27b10 it elided 5 reloads per hot
body and skipped no stores (`planned 0`): below noise, and the tree does not keep switches that never pay.

Census by the prof lane, cvm-rx27b9, default arm, the last body of each method (`/root/rx27/prof/lanes/sr/`):

| body | instructions | frame-slot instructions | adjacent store->reload | dead-store upper bound | non-zero stores | slot loads | zero clears |
|---|---|---|---|---|---|---|---|
| Start.match (matchonly, split-ws) | 1200 | 309 | 7 | 35 | 91 | 180 | 9 |
| BmpCharPropertyGreedy.match | 1794 | 605 | 9 | 119 | 162 | 275 | 111 |

Reading:

* The adjacent store->reload pair is rare: 7-9 per body.
* The dead-store column is an upper bound that ignores branches. Most of those pairs are merge phi copies through
  slots, which the edge rewrites on every incoming path.
* The traffic is mostly loads, about 2 per store: values homed in slots and re-read at each use. That is the
  accumulator lowering itself. Removing it needs operands read from their register or memory location in place,
  without the RAX round trip, and a phi copy that does not go through slots.
* `BmpCharPropertyGreedy.match`'s 111 zero clears (`mov qword [rbp-x],0`) are the bimodal lane's frame-clear
  lever, not this one.

## Close-out (2026-10-29): the frame-slot lane stops

The lane tried these levers. icount2 main/op is the median of 5 against the same binary's default, and "disjoint"
means the arm's range does not overlap the default's.

| switch | what it did | measured | outcome |
|---|---|---|---|
| `AND_ZERO_TEST_BRANCH` (azt) | `(x & m) == 0` branch as one `TEST`, `And` never computed | b8/b9: find-ci -12/op, the rest noise | opt-in |
| `AND_ZERO_TEST_GUARD` (azg) | same for pruned-branch guards, `And` rebuilt in the deopt pad | b9: find-email -1.0 % (disjoint) | opt-in |
| azt+azg+ebb (rall2) | the three together | b9: find-email -2.0 % (disjoint) | opt-in |
| `LS_CALLER_SAVED_EBB` (ebb) | R8-R11 across single-predecessor edges, whole-range homes dropped | b8: noise (1-2 values per hot body) | opt-in |
| `HOME_STORE_ELIDE` | reload behind its own store, unread stores | b10: 5 reloads per body, 0 stores | removed |
| `OPERAND_IN_PLACE` (oip) | second operand of compare / ALU from its slot | b11: no row disjoint; b15 psc+oip: compile -3.0 % (disjoint) | opt-in, oip-only arm pending |
| `PHI_SOURCE_COALESCE` (psc) | merge-phi source shares the phi's word | b15: noise on every row | removed |

**Why the accumulator lowering caps it.** The IR tier computes every value in RAX and writes it to its home, and
every consumer reads its first operand through RAX. In that model a register for a value turns a home load into a
register move, and a home store into a register publish. That is the same instruction count, which is what
`icount2` measures; W8-4 and W9-2 had already shown `REG_ALLOC` neutral.

The census confirms where the traffic is. In `Start.match`, 180 of 309 frame-slot instructions are loads, about 2
per store, and only 7-9 per body are adjacent store/reload pairs. Most loads are first-operand loads of that
accumulator shape. The per-character instructions go only when the lowering computes values in place: results
into their own registers, operands read where they live, no RAX round trip, and a home write only where a frame
state needs one. That is a lowering rewrite (a real register-allocated selector), not a pass over the current
one.

Open on this page: the oip-only arm, and a decision on keeping azt / azg / ebb as opt-in or removing them by the
same rule.

**2026-10-29, coordinator decision on b17:** `CRATONVM_JIT_IR_LS_CALLER_SAVED_EBB` is removed; it never paid alone
(b8 flat, and the rall2 gain is azt + azg's). `AND_ZERO_TEST_BRANCH`, `AND_ZERO_TEST_GUARD`, `OPERAND_IN_PLACE` and
`PUBLISH_RELOAD_ELIDE` flip to default ON together (rb against dse0: find-ci -2.8 %, find-email -4.8 %, both
disjoint, no row worse). The loop the EBB tests lowered survives as `jit/src/ir_lower/edge_loop_fixture.rs`.
