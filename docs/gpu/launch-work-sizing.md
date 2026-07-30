# Runtime GPU launch work sizing

Counted-loop kernels launch according to runtime work when it is available.
The dispatcher no longer takes `max(runtime_work, estimated_work)`, which used
to impose a 1,048,576-thread floor even for small arrays.

`vm/src/runtime/offload.rs` now uses a non-zero runtime work count directly
and falls back to the signature estimate only for scalar-only cases with no
runtime array length. A kernel over `N` elements therefore launches `N` work
items rather than an arbitrary estimate-sized grid.

See also [the historical validation record](../internal/fixed-suite-bugs/gpu-offload-followups-20260711.md#4-small-array-thread-over-launch-min-2⁰-threads-per-launch--done).
