# Runtime GPU launch work sizing

Counted-loop kernels launch according to runtime work when it is available.
The dispatcher no longer takes `max(runtime_work, estimated_work)`, which used
to impose a 1,048,576-thread floor even for small arrays.

`vm/src/runtime/offload.rs` now uses a non-zero runtime work count directly
and falls back to the signature estimate only for scalar-only cases with no
runtime array length. A kernel over `N` elements therefore launches `N` work
items rather than an arbitrary estimate-sized grid.

See also the historical validation record.

## A bound that is not an array length

`emitter::WorkBound` records where a counted loop's trip count came from, so
the grid can be sized from the loop rather than from the largest array
argument — the two differ by the inner dimension for a matrix-vector product,
where the largest-array rule launched 4.2 million threads to do 2,048
threads' work.

`WorkBound::ParamScalar` is the third variant and the only one the dispatch
site may not ignore. `for (int i = 0; i < n; i++)` with `n` an `int`
parameter compiles to bytecode identical to a hoisted `arr.length`, so the
recognizer tells them apart by what defined the local. Accepting it is not
free the way the inline-`arraylength` form was:

* a `ParamLen` bound is by construction no larger than the largest array
  argument, so falling back to the largest-array rule over-provisions, which
  is always safe;
* a scalar bound may be LARGER than every array the kernel was handed. A
  thread that is never created reaches no bounds check, so under-provisioning
  is not a deopt — it is a silently short result.

So `ParamScalar` carries the parameter index, the marshaller records every
`int` argument's value beside the array lengths it already records, and the
dispatch site sizes the grid from
`max(largest array length, that parameter's runtime value)`. If the scalar
did not reach the marshaller at all the launch is refused rather than
guessed at; falling back to the interpreter is always correct.

Only an UNMODIFIED parameter qualifies — the host sizes the grid from the
ARGUMENT it was handed, while the guard compares against whatever the local
holds at the header, and `n = n - 1` makes those two different numbers.

A 2-D rectangular nest bounded by two scalars is refused: its flattened trip
count is `rows * cols`, a product, and `WorkBound` names one parameter.
