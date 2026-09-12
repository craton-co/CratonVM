# FIXED: the IR builder refused bottom-tested (rotated) loops

**Status: FIXED 2026-09-12.** Found by the 2026-09-12 JIT review as finding #43
(missed optimization, not a correctness bug). A method refused here ran on the
single-pass tier.

## Shape

javac tests a `while`/`for` loop at the top. ECJ, Kotlin, Scala and several
bytecode generators rotate the loop instead: they jump forward to the test and
branch back to the body.

```text
  0: goto 12        // enter at the test
  3: <body>         // verifier "loop header": reached only from the back edge
 12: <cond>
 15: if<cond> 3
```

`IrBuilder::build` in `jit/src/ir.rs` walked bytecode in pc order. At pc 3 the
header had no visited forward entry, so `activate_loop_header` installed no
region, and the safety net refused the whole build with `ir_build_bail`.

## The fix, in the method's own code

`has_rotated_loop_header` asks whether any reachable header other than pc 0
has no predecessor at a lower pc. If one does, the method is walked in
reverse post-order of its block CFG, using the ranges
`BlockWalk::reverse_postorder` computes:

- **Loop headers** are the targets of edges that retreat in that order. In the
  shape above that is the test at pc 12, which dominates the body. The body
  becomes a one-predecessor merge.
- **Fall-through between ranges.** A fall-through out of a range into a block
  visited elsewhere is recorded as a merge predecessor, or back-patched when
  that block was already visited. Control and the abstract frame are then
  dropped, exactly as after a `goto`.
- **Every other method** keeps the pc walk, node for node.
- **`prune_always_taken_branch` is off under the block walk.** Its `pc = target`
  jump is a pc-order move.

## The fix, in spliced callee bodies

A callee body spliced by IR-tier inlining was still walked in pc order, so a
rotated loop inside it refused the CALLER's whole build.

- **Pre-scan.** The spliced-body pre-scan in `IrBuilder::build` runs
  `has_rotated_loop_header` over each body's own verified code. For a body with
  a rotated loop it computes that body's `BlockWalk`, rebases the ranges by the
  body's `base`, stores them in `splice_block_walks`, and registers the walk's
  loop headers instead of the verifier's.
- **Walk.** `begin_splice` copies the ranges onto the `SpliceFrame`
  (`ranges`, `next_range`, `range_end`). The build loop moves between a walked
  body's ranges the way it moves between the caller's, with the same
  merge-predecessor rule at a range end.
- **Exit.** Reverse post-order need not visit the body's trailing `return`
  last: in the shape above the return block comes before the loop body. A walked
  body therefore records each `return` as an exit edge, as a multi-return body
  does, and closes through `finish_multi_return_splice` when its ranges run out.

## Regression coverage

`jit/src/ir.rs`:

- build tests for an ECJ `for`, a do/while whose header is pc 0, a rotated loop
  nested in a javac loop, and a rotated loop with an if/else join in its body;
- `the_block_walk_visits_the_test_before_the_body_it_dominates`, which pins the
  ranges and headers and checks that javac shapes keep the pc walk;
- `a_spliced_callee_with_a_rotated_loop_builds_instead_of_refusing_the_caller`.

`jit/src/ir_lower.rs` runs the four caller shapes and
`a_spliced_callee_with_a_rotated_loop_runs` through build, optimize, schedule
and lower, and compares each result against the Java answer.

## Commits

58ede83bd (caller walk), 762ed3ced (spliced bodies, and the two run tests
that compared a zero-extended `int` result against a negative `i64`).
