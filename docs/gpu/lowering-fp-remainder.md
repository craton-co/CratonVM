# Floating-point remainder lowering

`frem` and `drem` lower to PTX arithmetic rather than a non-existent PTX
`rem.f32` or `rem.f64` instruction. The emitter constructs the Java remainder
identity from division, truncation, negation, and fused multiply-add.

This lowering is explicitly opt-in under `AdmissionHint::ALLOW_DIV_BY_ZERO`.
The strict analyzer path continues to reject float remainder because the
implementation is exact only for bounded quotients. Java floating-point zero
divisors do not need a deopt guard: IEEE division produces infinity or NaN.

Fixtures and `ptxas_round_trip_frem` cover the emitted sequence.
