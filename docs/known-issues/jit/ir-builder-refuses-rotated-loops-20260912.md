# The IR builder still refuses a rotated loop inside a spliced callee body

**Status:** PARTIAL (missed optimization, not a correctness bug). Found by the
2026-09-12 JIT review as finding #43. A method refused here still runs on the
single-pass tier.

The method's OWN bottom-tested loops build since 2026-09-12. What is left is
the same shape inside a callee body the IR tier inlines.

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

## What was fixed

`IrBuilder::build` in `jit/src/ir.rs` walked bytecode in pc order. At pc 3 the
header had no visited forward entry, so `activate_loop_header` installed no
region, and the safety net refused the whole build with `ir_build_bail`.

It now checks `has_rotated_loop_header` first. The check asks whether any
reachable header other than pc 0 has no predecessor at a lower pc. If one
does, the method is walked in reverse post-order of its block CFG, using the
ranges `BlockWalk::reverse_postorder` computes:

- **Loop headers.** They are the targets of edges that retreat in that order.
  In the shape above that is the test at pc 12, which dominates the body. The
  body becomes a one-predecessor merge.
- **Fall-through between ranges.** A fall-through out of a range into a block
  visited elsewhere is recorded as a merge predecessor, or back-patched when
  that block was already visited. Control and the abstract frame are then
  dropped, exactly as after a `goto`.
- **Every other method** keeps the pc walk, node for node.
- **`prune_always_taken_branch` is off under the block walk.** Its `pc = target`
  jump is a pc-order move.

Tests:

- `jit/src/ir.rs` covers each shape: an ECJ `for`, a do/while whose header is
  pc 0, a rotated loop nested in a javac loop, and a rotated loop with an
  if/else join in its body. Each builds with the expected loop merge.
  `the_block_walk_visits_the_test_before_the_body_it_dominates` pins the ranges
  and headers, and checks that javac shapes keep the pc walk.
- `jit/src/ir_lower.rs` runs all four shapes through build, optimize, schedule
  and lower. Each is called with `try_call` and compared against the Java
  answer.

## What remains

A callee body spliced by IR-tier inlining is still walked in pc order. The
walk enters it at the `invoke` and leaves it at its trailing return, and
`BlockWalk` covers only the caller's `code[..code_len]`.

The splice scanner in `jit_bridge.rs` admits branching bodies, loops included.
A body with a rotated loop therefore meets the original failure inside the
splice. Its header has no forward entry, the safety net fires, and the
CALLER's whole IR build is refused.

## The fix for what remains

Either of these would do:

- **Give each splice frame its own `BlockWalk`.** Compute it from the body's own
  verified code, rebased by `base`, in the spliced-body pre-scan. Walk the body's
  ranges while its frame is on top, and restore the caller's range position when
  the body returns. The range-end check in `build` is already gated on
  `self.splice.is_empty()`, and would need the same per-frame form.
- **Refuse the splice, not the method.** Make the splice scanner decline a body
  for which `has_rotated_loop_header` holds, so the site falls back to a call
  instead of refusing the caller.
