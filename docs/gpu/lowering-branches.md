# Acyclic loop-body branch lowering

The CUDA emitter lowers forward control flow inside a counted-loop body. It
discovers basic blocks, emits `L_body_<pc>` PTX labels, lowers
`if*`/`if_icmp*`/`ifnull`/`ifnonnull` to `setp` plus predicated branches, and
lowers forward `goto`/`goto_w` to PTX `bra` instructions.

Each target block has canonical registers for its live JVM locals and
operand-stack entries. Every incoming edge copies its state into those
registers, giving join points explicit phi-style semantics without relying on
the old linear simulated stack.

The one-thread-per-iteration model intentionally rejects interior backward
edges, branches leaving the loop body (`break`), and returns in the body.
Those shapes need a different iteration-space and completion model. Canonical
loop back-edges remain handled by the existing per-thread loop guard.

`EligibleBranchingLoop.java` includes an `if`/`else` local merge and a
one-arm branch. The `branching_loop_*` tests and
`ptxas_round_trip_branching_loops` cover emitted control-flow PTX.
