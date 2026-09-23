# Numeric constant-pool lowering

The CUDA bytecode analyzer and emitter support numeric `ldc`, `ldc_w`, and
`ldc2_w` instructions. When a constant pool is available, integer, long,
float, and double entries are resolved and emitted as PTX immediates.

This admits integer literals beyond `sipush` range as well as literal long,
float, and double values. Non-numeric constant-pool entries remain rejected.
The CP-free lowering entry point remains conservative and rejects these
opcodes because it cannot resolve their values safely.

Regression coverage includes `EligibleLdc*` fixtures and
`ptxas_round_trip_ldc_constants`.
