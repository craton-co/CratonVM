# Integer and long GPU reductions

GPU offload supports proven integer and long reductions whose method descriptor
returns `)I` or `)J`. The analyzer requires a counted loop, a loop-carried
accumulator that feeds the returned local, array reads, and no array store.
The interpreter receives the downloaded scalar through
`DispatchOutcome::HandledWithValue` in `vm/src/runtime/offload.rs`.

The PTX epilogue uses the one-operand reduction form
`red.global.add[.u64] [ptr], value;`. This matters: the older two-operand
`atom.global.add [ptr], value;` form was rejected by `ptxas`, silently forcing
CPU fallback. `ptxas_round_trip_dot_reduction` guards the emitted form.

Float and double reductions intentionally remain on CPU. Their atomic-add
order is not bit-identical to Java's sequential floating-point accumulation.

See also the historical validation record.
