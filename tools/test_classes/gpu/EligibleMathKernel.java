// AUDIT 2026-07-11: intrinsic-table follow-up. Every method here is
// analyzer-Ineligible under the default (`Strict`) verdict — each one's
// only obstacle is an `invokestatic` to `java/lang/Math`. They only
// become `Eligible` under `AdmissionHint::AllowIntrinsicCalls`, and (as
// with `EligibleFrem.java`) that hint is supplied by the Rust test code
// via `analyzer::analyze_with_annotations_and_pool` / the
// `lower_fixture_with_pool_and_hint` test helper — this fixture
// deliberately has NO package and imports nothing from the annotations
// jar, so it needs no extra annotation classpath to compile (mirrors
// `EligibleFrem.java`/`FloatRemainder.java`, not the real-`@GpuKernel`
// fixtures under `test_classes/gpu/annotations/`, e.g. `AdmitMathSqrt.java`,
// which exercise the same hint end-to-end through the actual annotation
// reader and `OffloadCache`).
//
// `powRejected` is the negative control: `Math.pow` is deliberately NOT
// in the curated intrinsic table (see
// `jit-cuda/src/analyzer.rs`'s `MathIntrinsic`/`resolve_math_intrinsic`)
// because PTX's `.approx` transcendentals don't meet Java's
// relative-error contract — it must still reject with `Reason::Invoke`
// even under the loosened hint.
public class EligibleMathKernel {
    // sqrt (Math.sqrt) + abs.f32 (Math.abs) + fma.rn.f32 (Math.fma) in one
    // canonical-loop body — the shape closing the documented
    // `ALLOW_INTRINSIC_CALLS` gap in `docs/gpu/annotations.md`.
    //
    // The sqrt here is the `f2d; sqrt(D)D; d2f` triple that EVERY float
    // square root in Java compiles to (Math.sqrt is declared only `(D)D`),
    // and it lowers to a single `sqrt.rn.f32` — see `float_sqrt_triple_at`.
    // `sqrtDouble` and `sqrtWidenedKept` below are the two shapes that must
    // NOT collapse.
    public static void sqrtAbsFma(
            float[] a, float[] b, float[] out, float x, float y, float z) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = (float) Math.sqrt((double) a[i]) * Math.abs(b[i]) + Math.fma(x, y, z);
        }
    }

    /** A genuine double square root: no widen, no narrow, stays f64. */
    public static void sqrtDouble(double[] a, double[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.sqrt(a[i]);
        }
    }

    /**
     * A float widened to double whose sqrt result is KEPT as a double:
     * `f2d; sqrt(D)D; dastore`, with no `d2f`. The double result is
     * observable, so collapsing to `sqrt.rn.f32` would change it. Must stay
     * `sqrt.rn.f64`.
     */
    public static void sqrtWidenedKept(float[] a, double[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.sqrt(a[i]);
        }
    }

    public static void absInt(int[] a, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.abs(a[i]);
        }
    }

    public static void absLong(long[] a, long[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.abs(a[i]);
        }
    }

    public static void fmaDouble(double[] a, double[] out, double x, double y, double z) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.abs(a[i]) + Math.fma(x, y, z);
        }
    }

    public static void minMaxInt(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.min(a[i], b[i]) + Math.max(a[i], b[i]);
        }
    }

    public static void minMaxLong(long[] a, long[] b, long[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.min(a[i], b[i]) + Math.max(a[i], b[i]);
        }
    }

    public static void minMaxFloat(float[] a, float[] b, float[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.min(a[i], b[i]) + Math.max(a[i], b[i]);
        }
    }

    public static void minMaxDouble(double[] a, double[] b, double[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.min(a[i], b[i]) + Math.max(a[i], b[i]);
        }
    }

    // Negative control — see the file doc comment above.
    public static void powRejected(double[] a, double[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = Math.pow(a[i], 2.0);
        }
    }
}
