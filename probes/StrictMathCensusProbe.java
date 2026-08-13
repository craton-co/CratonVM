// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Random;

/**
 * Census: for every {@code StrictMath} function CratonVM registers a native for,
 * how often does the fdlibm answer differ, BIT FOR BIT, from the platform-libm
 * answer?
 *
 * <p>The point of {@code StrictMath} is that its results are fixed by the fdlibm
 * algorithms and are therefore identical on every platform and every VM.
 * {@code Math} carries only a 1-ULP accuracy bound and is free to use the host's
 * libm or a CPU intrinsic. So on HotSpot, {@code StrictMath.f} IS fdlibm and
 * {@code Math.f} is (for most of this surface) not — and the per-function
 * disagreement rate between the two is a direct measurement of what a VM loses
 * by serving {@code StrictMath} from libm, which is what CratonVM was doing.
 *
 * <p>Caveat, stated so the number is read correctly: this measures HotSpot's
 * {@code Math} backing (x86-64 intrinsics plus the JDK's own Java fallbacks),
 * not the msvcrt/glibc routines Rust's {@code f64::sin} reaches. The two are not
 * the same libm, so this census establishes WHICH FUNCTIONS ARE AT RISK and
 * roughly how badly; the authoritative CratonVM-side number is the Rust-side
 * measurement in {@code types/src/fdlibm.rs}, which compares the port against
 * the very libm CratonVM links. Where {@code Math.f} on HotSpot simply calls
 * {@code StrictMath.f} (the JDK does that for several functions), this probe
 * reports 0% by construction — that is flagged in the output, not hidden.
 *
 * <p>Run: {@code java probes/StrictMathCensusProbe.java}
 */
public final class StrictMathCensusProbe {

    private static final int N = 1_000_000;

    interface Un { double apply(double x); }
    interface Bin { double apply(double x, double y); }

    private static long bits(double d) { return Double.doubleToRawLongBits(d); }

    /** Count of inputs where the two backings produce different bit patterns. */
    private static void un(String name, String domain, Un strict, Un loose, java.util.function.IntToDoubleFunction gen) {
        int diff = 0;
        long worstUlp = 0;
        double worstAt = 0;
        for (int i = 0; i < N; i++) {
            double x = gen.applyAsDouble(i);
            double a = strict.apply(x);
            double b = loose.apply(x);
            if (bits(a) != bits(b)) {
                if (Double.isNaN(a) && Double.isNaN(b)) continue;
                diff++;
                long u = ulpDistance(a, b);
                if (u > worstUlp) { worstUlp = u; worstAt = x; }
            }
        }
        report(name, domain, diff, worstUlp, worstAt, Double.NaN);
    }

    private static void bin(String name, String domain, Bin strict, Bin loose,
                            java.util.function.IntToDoubleFunction genX,
                            java.util.function.IntToDoubleFunction genY) {
        int diff = 0;
        long worstUlp = 0;
        double worstAtX = 0, worstAtY = 0;
        for (int i = 0; i < N; i++) {
            double x = genX.applyAsDouble(i);
            double y = genY.applyAsDouble(i);
            double a = strict.apply(x, y);
            double b = loose.apply(x, y);
            if (bits(a) != bits(b)) {
                if (Double.isNaN(a) && Double.isNaN(b)) continue;
                diff++;
                long u = ulpDistance(a, b);
                if (u > worstUlp) { worstUlp = u; worstAtX = x; worstAtY = y; }
            }
        }
        report(name, domain, diff, worstUlp, worstAtX, worstAtY);
    }

    /** Bit-distance between two finite doubles of the same sign convention. */
    private static long ulpDistance(double a, double b) {
        if (Double.isNaN(a) || Double.isNaN(b)) return -1;
        if (Double.isInfinite(a) || Double.isInfinite(b)) return -1;
        long ia = bits(a), ib = bits(b);
        if (ia < 0) ia = 0x8000000000000000L - ia;
        if (ib < 0) ib = 0x8000000000000000L - ib;
        return Math.abs(ia - ib);
    }

    private static void report(String name, String domain, int diff, long worstUlp, double atX, double atY) {
        double pct = 100.0 * diff / N;
        String at = Double.isNaN(atY) ? String.format("%.17g", atX)
                                      : String.format("%.17g, %.17g", atX, atY);
        System.out.printf("%-14s %-30s %9d / %d  %7.3f%%   worst=%d ULP%s%n",
                name, domain, diff, N, pct, worstUlp,
                diff == 0 ? "" : ("  at " + at));
    }

    public static void main(String[] args) {
        Random r = new Random(20260812L);
        // Pre-generate independent streams so every function sees the same
        // seeded sequence shape and the rates are comparable run to run.
        double[] u01 = new double[N];          // (0, 1)
        double[] pm1 = new double[N];          // [-1, 1]
        double[] trig = new double[N];         // [-2pi, 2pi]
        double[] trigBig = new double[N];      // [-1e6, 1e6] (heavy arg reduction)
        double[] expDom = new double[N];       // [-700, 700]
        double[] wide = new double[N];         // full positive exponent range
        double[] wideSigned = new double[N];   // full signed exponent range
        double[] small = new double[N];        // [-20, 20]
        double[] powBase = new double[N];      // (0, 100)
        double[] powExp = new double[N];       // [-20, 20]
        double[] wide2 = new double[N];        // second full-range stream

        for (int i = 0; i < N; i++) {
            u01[i] = r.nextDouble();
            if (u01[i] == 0.0) u01[i] = Double.MIN_NORMAL;
            pm1[i] = 2 * r.nextDouble() - 1;
            trig[i] = (2 * r.nextDouble() - 1) * 2 * Math.PI;
            trigBig[i] = (2 * r.nextDouble() - 1) * 1e6;
            expDom[i] = (2 * r.nextDouble() - 1) * 700;
            small[i] = (2 * r.nextDouble() - 1) * 20;
            powBase[i] = r.nextDouble() * 100 + 1e-3;
            powExp[i] = (2 * r.nextDouble() - 1) * 20;
            wide[i] = randomFinitePositive(r);
            wide2[i] = randomFinitePositive(r);
            wideSigned[i] = r.nextBoolean() ? -randomFinitePositive(r) : randomFinitePositive(r);
        }

        System.out.println("StrictMath (fdlibm) vs Math (platform/intrinsic) — bit disagreement");
        System.out.println("java.vendor=" + System.getProperty("java.vendor")
                + " java.version=" + System.getProperty("java.version")
                + " os=" + System.getProperty("os.name") + "/" + System.getProperty("os.arch"));
        System.out.printf("%-14s %-30s %9s   %s%n", "method", "domain", "differ", "rate");
        System.out.println("-".repeat(96));

        un("log", "(0,1) uniform", StrictMath::log, Math::log, i -> u01[i]);
        un("log", "full positive range", StrictMath::log, Math::log, i -> wide[i]);
        un("exp", "[-700,700]", StrictMath::exp, Math::exp, i -> expDom[i]);
        un("sin", "[-2pi,2pi]", StrictMath::sin, Math::sin, i -> trig[i]);
        un("sin", "[-1e6,1e6]", StrictMath::sin, Math::sin, i -> trigBig[i]);
        un("cos", "[-2pi,2pi]", StrictMath::cos, Math::cos, i -> trig[i]);
        un("cos", "[-1e6,1e6]", StrictMath::cos, Math::cos, i -> trigBig[i]);
        un("tan", "[-2pi,2pi]", StrictMath::tan, Math::tan, i -> trig[i]);
        un("tan", "[-1e6,1e6]", StrictMath::tan, Math::tan, i -> trigBig[i]);
        un("asin", "[-1,1]", StrictMath::asin, Math::asin, i -> pm1[i]);
        un("acos", "[-1,1]", StrictMath::acos, Math::acos, i -> pm1[i]);
        un("atan", "signed full range", StrictMath::atan, Math::atan, i -> wideSigned[i]);
        un("sqrt", "full positive range", StrictMath::sqrt, Math::sqrt, i -> wide[i]);
        un("cbrt", "signed full range", StrictMath::cbrt, Math::cbrt, i -> wideSigned[i]);
        un("log10", "full positive range", StrictMath::log10, Math::log10, i -> wide[i]);
        un("log1p", "(-1,1) via [-1,1]", StrictMath::log1p, Math::log1p, i -> pm1[i]);
        un("expm1", "[-20,20]", StrictMath::expm1, Math::expm1, i -> small[i]);
        un("sinh", "[-20,20]", StrictMath::sinh, Math::sinh, i -> small[i]);
        un("cosh", "[-20,20]", StrictMath::cosh, Math::cosh, i -> small[i]);
        un("tanh", "[-20,20]", StrictMath::tanh, Math::tanh, i -> small[i]);
        bin("atan2", "signed x signed", StrictMath::atan2, Math::atan2, i -> wideSigned[i], i -> wide[i]);
        bin("pow", "(0,100) ^ [-20,20]", StrictMath::pow, Math::pow, i -> powBase[i], i -> powExp[i]);
        bin("hypot", "positive x positive", StrictMath::hypot, Math::hypot, i -> wide[i], i -> wide2[i]);
        bin("IEEEremainder", "signed x positive", StrictMath::IEEEremainder, Math::IEEEremainder,
                i -> wideSigned[i], i -> wide[i]);

        System.out.println();
        System.out.println("NOTE: a 0.000% row means only that HotSpot's Math backing agrees with");
        System.out.println("fdlibm on this domain — for sqrt that is a THEOREM (IEEE 754 requires");
        System.out.println("sqrt to be exactly rounded, so every conforming implementation agrees);");
        System.out.println("for the others it is an empirical property of THIS host's Math backing");
        System.out.println("and says nothing about the libm a different VM links.");
    }

    /** A positive finite double drawn roughly uniformly over the exponent range. */
    private static double randomFinitePositive(Random r) {
        for (;;) {
            long b = r.nextLong() & 0x7FEF_FFFF_FFFF_FFFFL;
            double d = Double.longBitsToDouble(b);
            if (d > 0 && Double.isFinite(d)) return d;
        }
    }
}
