# Proposal: the loop-root rule can replace the `anewarray` loop-callee refusal

**Status:** OPEN (proposal, round rx27 wave 1, lane chain, 2026-10-28). Pending the rx27 build's A/B.

**Area:** the optimizing tier's splice admission. Planner: `jit/src/lib.rs` `mod rx27_loop_root`. VM resolver:
`vm/src/runtime/interpreter/jit_bridge.rs`, which calls `cratonvm_jit::ir::splice_anewarray_refuses_loop_callee`
(owned by lane B).

**Severity:** low. Both rules are opt-in, behind `CRATONVM_JIT_IR_SPLICE_ANEWARRAY`, which is not default ON.

## Evidence

W13-6 (`r23-w2-regex-rows-gap-is-per-call-cost-20261021.md`, "Round 25 wave 13") found two rows that lose when
a body with a loop moves up into a caller:

* `cached`: `Pattern.split(CharSequence,int,boolean)` moved up to a compile with `left=181`, and its in-loop
  `Matcher.find` and `subSequence` became calls (+12.6 %).
* `matchonly`: `Start.match` was spliced into `Matcher.search` pc 121 through receiver resolution (+8.3 %).

Round 16 wave c (`boxHelper(I)J` into `run(II)J`, `intValue()` lost its intrinsic) had the same shape.
W13-6's fix keys on the trigger of the first case: an `anewarray` callee with a back edge is kept as a call,
whether or not the caller could hold its loop. It cannot see the second case. It also refuses an `anewarray`
loop callee that a roomy caller would splice whole.

## Proposal

`CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE` asks the premise after the append. It counts the tree's rows under a
loop as resolved, as planned, and as really spliced. When the append lost some of them, it rolls back the
site. If the rx27 A/B shows that `lr` (`SPLICE_ANEWARRAY=1`, `SPLICE_ANEWARRAY_LOOP_CALLEES=1`,
`LOOP_ROOT_OWN_COMPILE=1`) matches `anew` + lane B's rule on `cached` and keeps `regex-matches` at 0.96x:

1. Make `LOOP_ROOT_OWN_COMPILE` default ON under `SPLICE_ANEWARRAY`.
2. Retire `splice_anewarray_refuses_loop_callee` (lane B's code) and its switch
   `CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES`.

## What would close it

* The A/B above.
* A broad compile battery with the rule on, showing it does not keep calls that its own compile does not
  repay. The rule judges every loop-rooted tree.
* Then the two steps above, or a measured reason to keep both rules.
