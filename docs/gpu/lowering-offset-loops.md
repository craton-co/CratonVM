# Non-zero-start counted loops

The loop recognizer accepts canonical `for (i = K; i < bound; i++)` loops
when `K` is a compile-time constant and non-negative. It records the start in
`CountedLoop::iv_start`, and the emitter folds that offset into the CUDA thread
index once in the kernel prologue.

Negative starts are rejected because the host's bound-sized launch would not
provide enough threads to cover the negative prefix. Non-unit or negative
strides also remain CPU fallbacks: the one-thread-per-iteration mapping only
models `i++` exactly.

`EligibleOffsetLoop.java` covers short-form and `ldc`-sourced starts; the
`offset_loop_*` and `ptxas_round_trip_offset_loops` tests cover lowering.
