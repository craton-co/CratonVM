// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * SBR3 lane lang: the cold, first-call cost of a short loop over the
 * {@code Integer} / {@code Long} / {@code Math} bit-op family. Under
 * {@code CRATONVM_PURE_BYTECODE=1} the interpreter runs the JDK's own bytecode for
 * these (a native row in the default), so the interpreted prefix before the first
 * OSR body dominates a loop of a few thousand iterations. Each loop body is its own
 * method and runs exactly once at n = 20000, so nothing has warmed it; the per-loop
 * and total times go to stderr, a checksum to stdout (the HotSpot diff).
 *
 * <pre>cratonvm --jdk-only -cp . Sbr3LangLeverBits</pre>
 */
public class Sbr3LangLeverBits {
    static long sink;

    static long bits(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += Integer.bitCount(i) + Long.numberOfTrailingZeros(i | 1024L) + Integer.reverseBytes(i);
        return s;
    }

    static long lz(int n) {
        long s = 0;
        for (int i = 1; i < n; i++) s += Integer.numberOfLeadingZeros(i) + Long.bitCount(i * 31L) + Integer.highestOneBit(i);
        return s;
    }

    static long sqrt(int n) {
        double s = 0;
        for (int i = 0; i < n; i++) s += Math.sqrt(i) + Math.abs(-i) + Math.max(i, 3) + Math.floorMod(i, 7);
        return (long) s;
    }

    static long rot(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += Integer.rotateLeft(i, 5) ^ Long.rotateRight(i, 3) + Integer.reverse(i) + Long.reverseBytes(i);
        return s;
    }

    interface L { long run(int n); }

    public static void main(String[] a) {
        String[] names = {"bits", "lz", "sqrt", "rot"};
        L[] loops = {Sbr3LangLeverBits::bits, Sbr3LangLeverBits::lz, Sbr3LangLeverBits::sqrt, Sbr3LangLeverBits::rot};
        final int n = 20000;
        long check = 0, total = 0;
        StringBuilder sb = new StringBuilder("TIME ns/op(first call, n=" + n + ")");
        for (int k = 0; k < loops.length; k++) {
            long t0 = System.nanoTime();
            check += loops[k].run(n);
            long dt = System.nanoTime() - t0;
            total += dt;
            sb.append(' ').append(names[k]).append('=').append(dt / n);
        }
        sink = check;
        System.err.println(sb + " total_ms=" + total / 1_000_000);
        System.out.println("CK Sbr3LangLeverBits sum=" + check);
    }
}
