# Curated Math and StrictMath intrinsics

`AdmissionHint::ALLOW_INTRINSIC_CALLS` does not admit arbitrary Java calls.
It resolves an `invokestatic` constant-pool target through the curated
`resolve_math_intrinsic` table, then lowers only a known PTX implementation.

Supported operations include the documented `sqrt`, `abs`, `min`, `max`, and
`fma` forms. Unknown methods and transcendental calls remain CPU fallbacks,
which keeps analyzer admission aligned with what the emitter can actually
lower.

See [annotations.md](annotations.md) for the user-facing hint and the complete
supported/excluded table. `ptxas_round_trip_math_intrinsics` validates the
emitted PTX.
