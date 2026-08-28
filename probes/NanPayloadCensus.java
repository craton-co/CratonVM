/**
 * Census of NaN *payload* fidelity across every route a NaN can take through
 * the VM: widening, narrowing, arithmetic, locals, arguments, returns, array
 * elements, static and instance fields, boxing, and `Math.scalb`.
 *
 * Companion to {@code F2dCensus.java}, which measures one route (f2d) against
 * the IEEE formats and needs no oracle. This one measures many routes and is
 * an oracle diff: run it on HotSpot and on CratonVM and compare the two
 * outputs byte for byte. Every line is `route pattern=hex -> hex`, so a diff
 * names the route and the pattern that disagree.
 *
 * Written for
 * `nan-payloads-lost-to-the-compactvalue-tag-collision-20260816`. The patterns
 * are chosen to straddle the CompactValue NaN-box tag space: a double collides
 * exactly when it is a negative quiet NaN with mantissa bit 50 set, i.e.
 * `bits & 0xFFFC_0000_0000_0000 == 0xFFFC_0000_0000_0000`. The list below
 * covers the boundary, the four the write-up pinned, the samples the f2d
 * census printed, and controls that must never have been affected (positive
 * NaNs, a negative NaN with the marker bit clear, the infinities).
 */
public final class NanPayloadCensus {

    /** Double bit patterns: the collision set, plus controls. */
    static final long[] DOUBLES = {
        0xFFFC000000000000L, // collision-set boundary
        0xFFFC000000000001L,
        0xFFFC541A80000000L, // f2d census sample, in=ffe2a0d4
        0xFFFDA846C0000000L, // f2d census sample, in=ffad4236
        0xFFFE5E0E80000000L, // f2d census sample, in=ffb2f074
        0xFFFFAF30A0000000L, // f2d census sample, in=fffd7985
        0xFFFFFFFFFFFFFFFFL, // all bits set
        0x7FF8000000000000L, // control: the canonical quiet NaN
        0x7FF8000000000001L, // control: a POSITIVE payload NaN
        0x7FFC000000000000L, // control: positive, marker bit set
        0xFFF8000000000000L, // control: NEGATIVE, marker bit CLEAR
        0xFFF0000000000001L, // control: negative signalling NaN
        0x7FF0000000000000L, // control: +inf
        0xFFF0000000000000L, // control: -inf
    };

    /** Float bit patterns: NaNs whose widening lands in the collision set, plus controls. */
    static final int[] FLOATS = {
        0xFFE2A0D4, // negative, mantissa bits 22 and 21 set -> collides
        0xFFB2F074,
        0xFFFD7985,
        0xFFAD4236,
        0xFFC00000, // control: negative quiet NaN, mantissa bit 21 clear
        0x7FC00000, // control: the canonical quiet float NaN
        0x7FE2A0D4, // control: the same payload, positive
        0x7F800001, // control: positive signalling NaN
        0xFF800001, // control: negative signalling NaN
    };

    static double staticField;
    double instanceField;

    static double identity(double d) {
        return d;
    }

    static double throughSixArgs(double a, long b, double c, int d, double e, Object f) {
        // Forces the callee's locals to hold two colliding doubles at once, at
        // slots the caller did not choose, with a category-1 slot between them.
        return a + (b == 0 ? 0.0 : 0.0) + (d == 0 ? c - c + e - e + a : a) + (f == null ? 0.0 : 0.0);
    }

    static void row(StringBuilder out, String route, long pattern, long got) {
        out.append(route).append(' ')
           .append(Long.toHexString(pattern)).append(" -> ")
           .append(Long.toHexString(got)).append('\n');
    }

    public static void main(String[] args) {
        StringBuilder out = new StringBuilder();
        NanPayloadCensus self = new NanPayloadCensus();

        double[] arr = new double[DOUBLES.length];
        Double[] boxes = new Double[DOUBLES.length];

        for (int i = 0; i < DOUBLES.length; i++) {
            long bits = DOUBLES[i];
            double d = Double.longBitsToDouble(bits);

            // The identity round-trip: longBitsToDouble then back.
            row(out, "raw-roundtrip", bits, Double.doubleToRawLongBits(d));

            // A local variable, read back.
            double local = d;
            row(out, "local", bits, Double.doubleToRawLongBits(local));

            // An argument and a return value.
            row(out, "arg-return", bits, Double.doubleToRawLongBits(identity(d)));
            row(out, "arg-return-6", bits,
                    Double.doubleToRawLongBits(throughSixArgs(d, 0L, d, 0, d, null)));

            // Arithmetic that must propagate the payload (IEEE 754 says the
            // result of an operation with one NaN operand is that NaN).
            row(out, "dmul1", bits, Double.doubleToRawLongBits(d * 1.0));
            row(out, "dadd0", bits, Double.doubleToRawLongBits(d + 0.0));
            row(out, "dsub0", bits, Double.doubleToRawLongBits(d - 0.0));
            row(out, "ddiv1", bits, Double.doubleToRawLongBits(d / 1.0));
            row(out, "dneg", bits, Double.doubleToRawLongBits(-d));

            // Narrowing to float and back is lossy by the formats, but it is
            // lossy the SAME way on every conforming VM.
            row(out, "d2f", bits, Float.floatToRawIntBits((float) d) & 0xffffffffL);
            row(out, "d2f2d", bits, Double.doubleToRawLongBits((double) (float) d));

            // Array element store and load.
            arr[i] = d;
            row(out, "array", bits, Double.doubleToRawLongBits(arr[i]));

            // Static and instance field store and load.
            staticField = d;
            row(out, "static-field", bits, Double.doubleToRawLongBits(staticField));
            self.instanceField = d;
            row(out, "instance-field", bits, Double.doubleToRawLongBits(self.instanceField));

            // Boxing.
            boxes[i] = Double.valueOf(d);
            row(out, "boxed", bits, Double.doubleToRawLongBits(boxes[i].doubleValue()));

            // The stack-shuffle opcodes: `d + d` needs dup2 in some shapes, and
            // an array store of a computed index exercises dup2_x1 / dup_x2.
            double[] one = new double[1];
            one[0] = d;
            double dup = one[0] * 1.0;
            row(out, "shuffle", bits, Double.doubleToRawLongBits(dup));

            // The canonicalizing conversion must still canonicalize.
            row(out, "canonical-bits", bits, Double.doubleToLongBits(d));

            // And the predicates must be unmoved by any of this.
            row(out, "isnan", bits, Double.isNaN(d) ? 1 : 0);
        }

        for (int fbits : FLOATS) {
            float f = Float.intBitsToFloat(fbits);
            long p = fbits & 0xffffffffL;

            row(out, "f-raw-roundtrip", p, Float.floatToRawIntBits(f) & 0xffffffffL);
            row(out, "f2d", p, Double.doubleToRawLongBits((double) f));
            row(out, "f2d2f", p, Float.floatToRawIntBits((float) (double) f) & 0xffffffffL);
            row(out, "fmul1", p, Float.floatToRawIntBits(f * 1.0f) & 0xffffffffL);

            // `Math.scalb(float, int)` is implemented in the JDK as
            // `(float)((double) f * 2^k)`, so the intermediate double passes
            // through a VM value slot. This is the one place the payload loss
            // had already been observed as a HotSpot disagreement (8 of 6000
            // census rows).
            for (int k : new int[] { -3, -1, 0, 1, 3 }) {
                row(out, "scalb" + k, p, Float.floatToRawIntBits(Math.scalb(f, k)) & 0xffffffffL);
            }

            float[] fa = new float[1];
            fa[0] = f;
            row(out, "f-array", p, Float.floatToRawIntBits(fa[0]) & 0xffffffffL);
            row(out, "f-boxed", p,
                    Float.floatToRawIntBits(Float.valueOf(f).floatValue()) & 0xffffffffL);
        }

        System.out.print(out);
        System.out.println("rows: " + out.toString().split("\n").length);
    }
}
