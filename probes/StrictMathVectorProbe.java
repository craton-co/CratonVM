// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Random;

/**
 * Emits the golden vectors for {@code types/src/fdlibm.rs}, measured on HotSpot.
 *
 * <p>Two kinds of input, because they fail differently:
 *
 * <ul>
 * <li><b>Branch boundaries.</b> Every fdlibm routine is a decision tree over the
 *     high word — {@code |x| < 2^-27}, {@code |x| >= 0.6744}, {@code |x| > 22},
 *     {@code hx <= 0xbfd2bec3} — and a port that takes one wrong branch is
 *     correct almost everywhere and wrong on a set a uniform sample never
 *     visits. Each constant below sits ON such a boundary, or one ULP either
 *     side of it.
 * <li><b>Seeded pseudorandom draws</b> across the meaningful domain, which
 *     catch a mistyped polynomial coefficient that no boundary touches.
 * </ul>
 *
 * <p>The output is Rust source: a {@code &[(u64, u64)]} per unary function and a
 * {@code &[(u64, u64, u64)]} per binary one, all as raw bit patterns. Bits, not
 * decimals — the whole contract is the last bit, and a decimal round-trip is a
 * place for it to be lost.
 *
 * <p>Run: {@code java probes/StrictMathVectorProbe.java > vectors.rs}
 */
public final class StrictMathVectorProbe {

    interface Un { double apply(double x); }
    interface Bin { double apply(double x, double y); }

    private static final double[] UNIVERSAL = {
        0.0, -0.0, 1.0, -1.0, 2.0, -2.0, 0.5, -0.5,
        Double.MIN_VALUE, -Double.MIN_VALUE,
        Double.MIN_NORMAL, -Double.MIN_NORMAL,
        Math.nextDown(Double.MIN_NORMAL),           // largest subnormal
        Double.MAX_VALUE, -Double.MAX_VALUE,
        Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, Double.NaN,
        Math.nextDown(1.0), Math.nextUp(1.0),
    };

    /** Concatenate the universal edge set with function-specific boundaries. */
    private static double[] edges(double... extra) {
        double[] out = new double[UNIVERSAL.length + extra.length * 3];
        int n = 0;
        for (double d : UNIVERSAL) out[n++] = d;
        for (double d : extra) {
            out[n++] = d;
            out[n++] = Math.nextUp(d);
            out[n++] = Math.nextDown(d);
        }
        double[] trimmed = new double[n];
        System.arraycopy(out, 0, trimmed, 0, n);
        return trimmed;
    }

    private static final Map<String, List<double[]>> BINARY = new LinkedHashMap<>();
    private static final Map<String, List<Double>> UNARY = new LinkedHashMap<>();

    private static void emitUnary(String name, Un f, double[] edge, int nRandom,
                                  java.util.function.DoubleSupplier gen) {
        List<Double> xs = new ArrayList<>();
        for (double d : edge) xs.add(d);
        for (int i = 0; i < nRandom; i++) xs.add(gen.getAsDouble());
        System.out.printf("    /// `StrictMath.%s`, %d vectors from Temurin 25.0.3+9.%n", name, xs.size());
        System.out.printf("    const %s_VECTORS: &[(u64, u64)] = &[%n", name.toUpperCase());
        for (double x : xs) {
            System.out.printf("        (0x%016X, 0x%016X),%n",
                    Double.doubleToRawLongBits(x), Double.doubleToRawLongBits(f.apply(x)));
        }
        System.out.println("    ];");
        System.out.println();
    }

    private static void emitBinary(String name, Bin f, double[][] pairs) {
        System.out.printf("    /// `StrictMath.%s`, %d vectors from Temurin 25.0.3+9.%n", name, pairs.length);
        System.out.printf("    const %s_VECTORS: &[(u64, u64, u64)] = &[%n", name.toUpperCase());
        for (double[] p : pairs) {
            System.out.printf("        (0x%016X, 0x%016X, 0x%016X),%n",
                    Double.doubleToRawLongBits(p[0]), Double.doubleToRawLongBits(p[1]),
                    Double.doubleToRawLongBits(f.apply(p[0], p[1])));
        }
        System.out.println("    ];");
        System.out.println();
    }

    private static Random r;

    private static double widePos() {
        for (;;) {
            long b = r.nextLong() & 0x7FEF_FFFF_FFFF_FFFFL;
            double d = Double.longBitsToDouble(b);
            if (d > 0 && Double.isFinite(d)) return d;
        }
    }

    private static double wideSigned() { return r.nextBoolean() ? -widePos() : widePos(); }

    /** Cross the edge set with itself plus random pairs, for the binary functions. */
    private static double[][] pairs(double[] a, double[] b, int nRandom,
                                    java.util.function.DoubleSupplier gx,
                                    java.util.function.DoubleSupplier gy) {
        List<double[]> out = new ArrayList<>();
        for (int i = 0; i < Math.max(a.length, b.length); i++) {
            out.add(new double[]{a[i % a.length], b[(i * 7 + 3) % b.length]});
        }
        for (int i = 0; i < nRandom; i++) out.add(new double[]{gx.getAsDouble(), gy.getAsDouble()});
        return out.toArray(new double[0][]);
    }

    public static void main(String[] args) {
        r = new Random(0x5EEDL);
        double pi4 = Math.PI / 4, pi2 = Math.PI / 2, ln2 = StrictMath.log(2.0);

        System.out.println("// GENERATED by probes/StrictMathVectorProbe.java against Temurin");
        System.out.println("// jdk-25.0.3+9. Regenerate with that probe rather than editing: the");
        System.out.println("// point of these tables is that they are the reference implementation's");
        System.out.println("// output, not this port's.");
        System.out.println();

        // sin/cos/tan: the reduction thresholds are pi/4, 3pi/4, 2^19*(pi/2)
        // and the kernels switch at 2^-27/2^-28, 0.3 and 0.6744.
        double[] trigEdge = edges(pi4, pi2, 3 * pi4, Math.PI, 2 * Math.PI,
                0x1.0p-27, 0x1.0p-28, 0.3, 0.6744, 0.78125,
                0x1.0p19 * pi2, 0x1.0p20, 1e6, 1e22, 0x1.0p66);
        emitUnary("sin", StrictMath::sin, trigEdge, 24, () -> (2 * r.nextDouble() - 1) * 2 * Math.PI);
        emitUnary("cos", StrictMath::cos, trigEdge, 24, () -> (2 * r.nextDouble() - 1) * 2 * Math.PI);
        emitUnary("tan", StrictMath::tan, trigEdge, 24, () -> (2 * r.nextDouble() - 1) * 2 * Math.PI);

        // asin/acos: 0.5 and 0.975 pick the branch; |x| > 1 is NaN.
        double[] asinEdge = edges(0.5, 0.975, 0x1.0p-27, 0x1.0p-57, 1.0, -1.0, 1.5);
        emitUnary("asin", StrictMath::asin, asinEdge, 24, () -> 2 * r.nextDouble() - 1);
        emitUnary("acos", StrictMath::acos, asinEdge, 24, () -> 2 * r.nextDouble() - 1);

        // atan: 0.4375, 0.6875, 1.1875, 2.4375, 2^66, 2^-29.
        emitUnary("atan", StrictMath::atan,
                edges(0.4375, 0.6875, 1.1875, 2.4375, 1.5, 0x1.0p66, 0x1.0p-29),
                24, StrictMathVectorProbe::wideSigned);

        // exp: 0.5ln2, 1.5ln2, 2^-28, the overflow and underflow thresholds.
        emitUnary("exp", StrictMath::exp,
                edges(0.5 * ln2, 1.5 * ln2, 0x1.0p-28, 709.782712893384, -745.1332191019411,
                      -708.0, 710.0, 1.0, -1.0),
                24, () -> (2 * r.nextDouble() - 1) * 700);

        // cbrt: subnormals take the B2 path, everything else B1.
        emitUnary("cbrt", StrictMath::cbrt,
                edges(8.0, -8.0, 27.0, 0x1.0p-1022, 0x1.0p1023, 1e-300, 1e300),
                24, StrictMathVectorProbe::wideSigned);

        // log10 / log1p / expm1: the log1p branch constants are the subtle ones
        // (0.41422, -0.2929, 2^-29, 2^-54).
        emitUnary("log10", StrictMath::log10,
                edges(1.0, 10.0, 100.0, 1e-300, 1e300, 0x1.0p-1022), 24,
                StrictMathVectorProbe::widePos);
        emitUnary("log1p", StrictMath::log1p,
                edges(0.41422, -0.2929, 0x1.0p-29, 0x1.0p-54, -1.0, 1.0, 1e300,
                      Math.pow(2, 52), Math.pow(2, 53)),
                24, () -> 2 * r.nextDouble() - 1);
        emitUnary("expm1", StrictMath::expm1,
                edges(0.5 * ln2, 1.5 * ln2, 56 * ln2, 0x1.0p-54, -0.25, 709.782712893384,
                      -56 * ln2, 1.0, -1.0, 20.0, 56.0),
                24, () -> (2 * r.nextDouble() - 1) * 20);

        // sinh/cosh/tanh: 22, 0.5ln2, 2^-28, 2^-55, log(MAX), the overflow edge.
        double[] hypEdge = edges(22.0, -22.0, 0.5 * ln2, 0x1.0p-28, 0x1.0p-55,
                709.782712893384, 710.4758600739439, 1.0, -1.0);
        emitUnary("sinh", StrictMath::sinh, hypEdge, 24, () -> (2 * r.nextDouble() - 1) * 20);
        emitUnary("cosh", StrictMath::cosh, hypEdge, 24, () -> (2 * r.nextDouble() - 1) * 20);
        emitUnary("tanh", StrictMath::tanh, hypEdge, 24, () -> (2 * r.nextDouble() - 1) * 20);

        // atan2: the full sign/infinity matrix plus the 2^60 ratio cutoffs.
        emitBinary("atan2", StrictMath::atan2,
                pairs(edges(1.0, 0.0, 1e-300, 1e300), edges(1.0, 0.0, 1e-300, 1e300), 32,
                        StrictMathVectorProbe::wideSigned, StrictMathVectorProbe::wideSigned));

        // pow: the y == 2 / y == 0.5 / |y| == 1 / |y| == inf shortcuts, the
        // odd-vs-even integer exponent split for negative bases, and 2^53.
        emitBinary("pow", StrictMath::pow,
                pairs(edges(2.0, -2.0, 0.5, 1.0, -1.0, 10.0, 0.0),
                      edges(2.0, 0.5, 1.0, -1.0, 3.0, -3.0, 0x1.0p53, 0x1.0p31, 0.0),
                      40, () -> r.nextDouble() * 100 + 1e-3,
                      () -> (2 * r.nextDouble() - 1) * 20));

        // hypot: the 2^500 / 2^-500 rescaling ladder and the 2^60 ratio cutoff.
        emitBinary("hypot", StrictMath::hypot,
                pairs(edges(3.0, 4.0, 0x1.0p500, 0x1.0p-500, 0x1.0p-1022, 0.0),
                      edges(3.0, 4.0, 0x1.0p500, 0x1.0p-500, 0x1.0p-1022, 0.0),
                      32, StrictMathVectorProbe::widePos, StrictMathVectorProbe::widePos));

        // IEEEremainder: ties-to-even is the whole point, so the half-integer
        // quotients are the vectors that matter.
        emitBinary("IEEEremainder", StrictMath::IEEEremainder,
                pairs(edges(1.5, 2.5, 3.0, -3.0, 0.0, 1e300, 0x1.0p-1022),
                      edges(1.0, 2.0, 3.0, 0x1.0p-1022, 1e300),
                      40, StrictMathVectorProbe::wideSigned, StrictMathVectorProbe::widePos));
    }
}
