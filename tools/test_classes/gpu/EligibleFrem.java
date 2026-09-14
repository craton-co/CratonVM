// AUDIT 2026-07-11: unlike every other `Eligible*.java` fixture in this
// directory, `frem` is NOT eligible under the default (`Strict`)
// analyzer verdict — `analyze()` / `analyze_with_pool()` reject it with
// `Reason::FloatRemainder`, exactly like `FloatRemainder.java`. It only
// becomes `Eligible` under `AdmissionHint::AllowDivByZero`, which the
// analyzer reuses to gate `frem`/`drem` admission (see
// `analyzer::classify`'s `0x72 | 0x73` arm) because the PTX lowering
// (`lowering::emit::Emitter::frem_f32`) is bit-exact only while the
// quotient `|dividend / divisor|` stays within `float`'s
// exactly-representable-integer range — see that function's doc
// comment for the full precision analysis and the deliberately-scoped
// list of edge cases it does and does not handle exactly.
//
// `3.7f` has no `fconst` short form (only `fconst_0`/`_1`/`_2` exist),
// so it compiles to `ldc` (0x12) — this fixture is admissible only
// through the pool-aware AND hint-aware entry point,
// `analyzer::analyze_with_annotations_and_pool`, with both loosenings
// applied at once: the ldc-numeric-literal resolution (AUDIT C31
// follow-up) and `AllowDivByZero`.
public class EligibleFrem {
    public static void frem(float[] in, float[] out) {
        int n = in.length;
        for (int i = 0; i < n; i++) {
            out[i] = in[i] % 3.7f;
        }
    }
}
