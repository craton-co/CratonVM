# IR frame states record no monitor stack, so monitor-bearing methods cannot resume precisely

**Status:** OPEN (lost precision; the fallback stays correct). Found by the
2026-09-12 JIT review.

## Where

- `jit/src/ir_lower.rs`: every `FrameState` the lowerer builds hard-codes
  `monitors: Vec::new()`.
- The same file, at the point that routes a method through precise resume
  (`can_deopt_resume`). It refuses when the graph holds a monitor.

## Consequence

A method compiled by the optimizing tier that takes a monitor can still
deoptimize. It cannot resume *precisely*. It falls back to a whole-method
re-run, which re-enters its re-entrant lock and stays balanced. That fallback
is correct only because nothing before the deopt point has escaped. The
precise path exists for the methods where that is not true, and those are
exactly the methods it cannot serve here.

Two related fixes already landed on 2026-09-12:

- Lock elision used to delete the monitors before this check ran, which
  re-enabled precise resume on a graph that had held them. The lowerer now
  latches "had monitors" before escape analysis.
- The monitor lowering itself was repaired: MonitorExit no longer writes its
  return value into the object's home.

## The fix

1. Carry the monitor stack in the builder's abstract state. `monitorenter`
   pushes the locked reference and `monitorexit` pops it, and each snapshot
   copies the stack into its `FrameState`.
2. Map each entry to a `FrameValue` the same way a local is mapped: a register,
   a stack slot, or a virtual object when the lock was elided. An elided lock
   has to be re-acquired on materialization, as HotSpot's relock-on-deopt does
   (`lock-elimination.md`).
3. Delete the refusal once the interpreter's resume sink receives real monitor
   state. That sink already refuses frames it cannot rebuild.
