# The IR builder refuses every method with a bottom-tested (rotated) loop

**Status:** OPEN (missed optimization, not a correctness bug). Found by the
2026-09-12 JIT review. Such a method still runs on the single-pass tier.

## Shape

javac tests a `while`/`for` loop at the top. ECJ, Kotlin, Scala and several
bytecode generators rotate the loop instead: they jump forward to the test and
branch back to the body.

```text
  0: goto 12        // enter at the test
  3: <body>         // loop header: reached only from the back edge
 12: <cond>
 15: if<cond> 3
```

## Where it fails

`IrBuilder::build` in `jit/src/ir.rs` visits bytecode in linear pc order.

1. At pc 3, the header is in `loop_headers`. Its only predecessor, the branch
   at pc 15, has not been visited yet, and the `goto` at pc 0 targets pc 12,
   not pc 3.
2. So `activate_loop_header` finds no control input and returns without
   installing a region.
3. The safety net right after the merge activation then sees no live control
   token and returns `ir_build_bail(line!(), pc)`.

The whole IR build is refused, so the method never reaches the optimizing tier.

## Why it was not fixed in the review

The builder's merge bookkeeping assumes that every loop header has at least
one forward entry already visited. Loop-carried phis, eager local phis and
back-edge patching all depend on that. There are two clean fixes, and both
change the builder's core walk:

- **Walk blocks in reverse post-order** of the bytecode CFG, rather than pc
  order. A header is then visited after its entry block. That entry is the
  `goto` target (the test), which already dominates the body.
- **Seed a rotated header from its dominating entry.** When a header has no
  visited forward entry but its natural loop is entered through the test
  block, build the test block first and treat its fall-through or branch edge
  into the body as the forward entry.

Either fix needs the builder's existing test suite plus new rotated-loop
fixtures: an ECJ-shaped `for`, a `do/while`, and a nested rotated loop inside
a javac loop.
