// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.BufferedWriter;
import java.io.IOException;
import java.io.OutputStreamWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Random;

/**
 * Dumps a machine-readable fdlibm oracle: for each {@code StrictMath} function,
 * N sampled inputs and the exact bits {@code StrictMath} returns for them.
 *
 * <p>This exists because the interesting comparison is not the one a Java
 * program can make. {@code StrictMathCensusProbe} compares fdlibm against
 * HotSpot's {@code Math} backing, and HotSpot's {@code Math} is a SOURCE-LEVEL
 * DELEGATE to {@code StrictMath} for the whole transcendental surface — the two
 * differ only where C2 substitutes an x86 intrinsic. So a 0% row there means
 * "no intrinsic on this host", NOT "libm agrees with fdlibm", and it says
 * nothing about the C runtime a native VM links.
 *
 * <p>CratonVM's transcendentals are Rust's {@code f64::sin} and friends, which
 * are the host C library's. To measure THAT against fdlibm, the oracle has to
 * leave the JVM. This probe writes it out; the Rust side reads the file and
 * compares. Format, one line per sample:
 *
 * <pre>{@code   <fn> <argbits-hex> [<arg2bits-hex>] <resultbits-hex> }</pre>
 *
 * <p>Run: {@code java probes/StrictMathOracleDumpProbe.java <outfile> [n]}
 */
public final class StrictMathOracleDumpProbe {

    interface Un { double apply(double x); }
    interface Bin { double apply(double x, double y); }

    private static BufferedWriter out;
    private static int n = 200_000;
    private static Random r;

    private static void un(String name, Un f, java.util.function.DoubleSupplier gen) throws IOException {
        for (int i = 0; i < n; i++) {
            double x = gen.getAsDouble();
            out.write(name);
            out.write(' ');
            out.write(Long.toHexString(Double.doubleToRawLongBits(x)));
            out.write(' ');
            out.write(Long.toHexString(Double.doubleToRawLongBits(f.apply(x))));
            out.write('\n');
        }
    }

    private static void bin(String name, Bin f, java.util.function.DoubleSupplier gx,
                            java.util.function.DoubleSupplier gy) throws IOException {
        for (int i = 0; i < n; i++) {
            double x = gx.getAsDouble();
            double y = gy.getAsDouble();
            out.write(name);
            out.write(' ');
            out.write(Long.toHexString(Double.doubleToRawLongBits(x)));
            out.write(' ');
            out.write(Long.toHexString(Double.doubleToRawLongBits(y)));
            out.write(' ');
            out.write(Long.toHexString(Double.doubleToRawLongBits(f.apply(x, y))));
            out.write('\n');
        }
    }

    /** A positive finite double drawn roughly uniformly over the exponent range. */
    private static double widePos() {
        for (;;) {
            long b = r.nextLong() & 0x7FEF_FFFF_FFFF_FFFFL;
            double d = Double.longBitsToDouble(b);
            if (d > 0 && Double.isFinite(d)) return d;
        }
    }

    private static double wideSigned() {
        return r.nextBoolean() ? -widePos() : widePos();
    }

    public static void main(String[] args) throws IOException {
        Path p = Path.of(args.length > 0 ? args[0] : "fdlibm-oracle.txt");
        if (args.length > 1) n = Integer.parseInt(args[1]);
        r = new Random(20260812L);

        try (BufferedWriter w = new BufferedWriter(
                new OutputStreamWriter(Files.newOutputStream(p), StandardCharsets.US_ASCII), 1 << 20)) {
            out = w;
            // Domains chosen so each function is exercised where it is actually
            // used, and where its argument reduction has real work to do.
            un("log", StrictMath::log, StrictMathOracleDumpProbe::widePos);
            un("log_u01", StrictMath::log, () -> Math.max(r.nextDouble(), Double.MIN_NORMAL));
            un("exp", StrictMath::exp, () -> (2 * r.nextDouble() - 1) * 700);
            un("sin", StrictMath::sin, () -> (2 * r.nextDouble() - 1) * 2 * Math.PI);
            un("sin_big", StrictMath::sin, () -> (2 * r.nextDouble() - 1) * 1e6);
            un("cos", StrictMath::cos, () -> (2 * r.nextDouble() - 1) * 2 * Math.PI);
            un("cos_big", StrictMath::cos, () -> (2 * r.nextDouble() - 1) * 1e6);
            un("tan", StrictMath::tan, () -> (2 * r.nextDouble() - 1) * 2 * Math.PI);
            un("tan_big", StrictMath::tan, () -> (2 * r.nextDouble() - 1) * 1e6);
            un("asin", StrictMath::asin, () -> 2 * r.nextDouble() - 1);
            un("acos", StrictMath::acos, () -> 2 * r.nextDouble() - 1);
            un("atan", StrictMath::atan, StrictMathOracleDumpProbe::wideSigned);
            un("sqrt", StrictMath::sqrt, StrictMathOracleDumpProbe::widePos);
            un("cbrt", StrictMath::cbrt, StrictMathOracleDumpProbe::wideSigned);
            un("log10", StrictMath::log10, StrictMathOracleDumpProbe::widePos);
            un("log1p", StrictMath::log1p, () -> 2 * r.nextDouble() - 1);
            un("expm1", StrictMath::expm1, () -> (2 * r.nextDouble() - 1) * 20);
            un("sinh", StrictMath::sinh, () -> (2 * r.nextDouble() - 1) * 20);
            un("cosh", StrictMath::cosh, () -> (2 * r.nextDouble() - 1) * 20);
            un("tanh", StrictMath::tanh, () -> (2 * r.nextDouble() - 1) * 20);
            bin("atan2", StrictMath::atan2, StrictMathOracleDumpProbe::wideSigned,
                    StrictMathOracleDumpProbe::widePos);
            bin("pow", StrictMath::pow, () -> r.nextDouble() * 100 + 1e-3,
                    () -> (2 * r.nextDouble() - 1) * 20);
            bin("hypot", StrictMath::hypot, StrictMathOracleDumpProbe::widePos,
                    StrictMathOracleDumpProbe::widePos);
            bin("IEEEremainder", StrictMath::IEEEremainder,
                    StrictMathOracleDumpProbe::wideSigned, StrictMathOracleDumpProbe::widePos);
        }
        System.out.println("wrote " + p.toAbsolutePath() + " (" + Files.size(p) + " bytes, n=" + n + " per function)");
    }
}
