# Nested loops: the two mappings

The lowering has two ways to handle a loop nest, and they are not
interchangeable — they parallelise different dimensions.

## Rectangular two-dimensional flattening

A canonical two-level rectangular loop whose inner loop is the outer
loop's entire body and whose induction variables start at zero is
flattened to `[0, R*C)`, and `i = tid / C`, `j = tid % C` are recovered
in the kernel through `Emitter::emit_nested_loop_guard_and_decompose`.
Both bounds must be a literal or an array-parameter length hoisted to a
local.

One thread per `(i, j)` pair. This is right for an element-wise kernel
over a matrix and wrong for anything with a loop-carried value.

`EligibleNestedLoop.java`, `nested_loop_*`, and
`ptxas_round_trip_nested_loop` cover the accepted shape.

## Outer-parallel with a sequential inner loop

When the flattening does not apply — the outer body holds more than the
inner loop, or the inner loop carries an accumulator —
`loop_recog::classify_outer_parallel_loop` takes over: the outermost
back-edge is validated as the canonical counted loop and becomes the
parallel dimension, and every other back-edge inside its body is
lowered by `Emitter::walk_cfg` as a real PTX loop the thread runs
itself.

One thread per outer iteration. This is the shape a matrix-vector
product has:

```java
for (int i = 0; i < rows; i++) {
    float sum = 0.0f;
    for (int j = 0; j < n; j++) {
        sum += w[i * n + j] * x[j];
    }
    out[i] = sum;
}
```

`sum` is loop-carried, so the `j` iterations must run in order on one
thread; there is no assignment of `(i, j)` pairs to threads that
computes it.

The machinery this needed was mostly already there. `walk_cfg` visits
blocks in ascending PC order and reconciles JVM state at joins, so a
back-edge target is a block whose canonical registers already exist and
whose label is already written by the time the edge is emitted — and
those registers ARE the loop's phis. What it did not have: a label on
the body's first block when the inner loop's header is the whole outer
body; a refusal for a back-edge into a block the walk never entered
forwards (irreducible or unreachable — emitting `bra` to a label that
does not exist is a `ptxas` failure rather than the CPU fallback every
other refusal produces); and simultaneous state copies at a join, since
a parallel assignment emitted one copy at a time is only equivalent
when no destination is also somebody else's source.

`EligibleRowReduction.java` and `EligibleSplitMatmul.java` are the
fixtures; `row_reduction_*`, `split_matmul_*`,
`ptxas_round_trip_row_reduction` and `ptxas_round_trip_split_matmul`
cover them.

## Which one you want

For a decode step in a language model, the answer is the second, with
the summed dimension split so the launch is wide enough to fill the
device — see `EligibleSplitMatmul.matmulColSplit`. One thread per
output row of a 2048-row projection is 64 warps, which on 30 SMs is two
warps each and no way to hide a global-load latency; measured on an RTX
2060, the same kernel over 2048 rows reaches 7 GB/s and over 32768 rows
reaches 99 GB/s.

Three or more nesting levels, and any loop nest whose back-edges do not
strictly contain one another (two sequential loops), remain CPU
fallbacks.
