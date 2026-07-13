# Rectangular two-dimensional loop lowering

The lowering supports a canonical two-level rectangular loop whose inner loop
is the outer loop's entire body and whose induction variables start at zero.
It flattens the iteration space to `[0, R*C)`, then recovers `i = tid / C` and
`j = tid % C` in the kernel through
`Emitter::emit_nested_loop_guard_and_decompose`.

Both bounds must be a literal or an array-parameter length hoisted to a local.
Triangular loops, sequential loops, and three-or-more levels remain CPU
fallbacks because their iteration spaces are not represented by this mapping.

`EligibleNestedLoop.java`, `nested_loop_*`, and
`ptxas_round_trip_nested_loop` cover the accepted shape.
