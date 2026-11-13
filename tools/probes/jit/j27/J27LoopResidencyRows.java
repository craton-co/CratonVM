// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * j27 wave 2 (lane RESIDENCY): fact rows for the loop-aware spill cost
 * (CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY, opt-in), page
 * docs/feature-designs/jit-ir-optimizations.md (item 2).
 *
 * Loops whose carried int / long values outnumber the five-register GP file, with
 * loop invariants that are not parameters (the values the switch makes yield):
 * pure int and long accumulation; a call inside the loop; an exception thrown
 * inside the loop and caught in the same method (the handler reads the carried
 * locals); a virtual call devirtualised by CHA whose receiver class is first
 * LOADED mid-loop (the running body is invalidated and deoptimises with its
 * carried values live); an allocating loop (GC points with carried values).
 * Every answer is Java int / long arithmetic, so the expected lines are HotSpot's.
 *
 * java J27LoopResidencyRows               facts only (deterministic)
 * java -Dj27.time=1 J27LoopResidencyRows  also "time <row> <ns>" lines
 */
public class J27LoopResidencyRows {
    static int pressureInt(int n) {
        int k = n * 3, m = n + 7;
        int a = 0, b = 1, c = 2;
        for (int i = 0; i < n; i++) {
            a += i ^ k;
            b += a + m;
            c ^= b + k;
        }
        return a + b + c;
    }

    static long cellsLong(int n, long k, long bias) {
        long v = 1, w = 2, x = 3;
        long ks = k * 977;
        for (int i = 0; i < n; i++) {
            v += ks + i;
            w += bias ^ v;
            x = x * 31 + (w >>> 7);
        }
        return v ^ w ^ x;
    }

    static long callLoop(int n) {
        int a = 0, b = 1, c = 2;
        for (int i = 0; i < n; i++) {
            int len = String.valueOf(i).length();
            a += len * i;
            b ^= a + len;
            c += b;
        }
        return ((long) a * 31 + b) * 31 + c;
    }

    static String throwInside(int n, int at) {
        int a = 0, b = 1, c = 2;
        int i = 0;
        try {
            for (; i < n; i++) {
                a += 1000 / (at - i);
                b ^= a;
                c += b;
            }
        } catch (ArithmeticException e) {
            return "caught i=" + i + " a=" + a + " b=" + b + " c=" + c + " " + e.getMessage();
        }
        return "done a=" + a + " b=" + b + " c=" + c;
    }

    abstract static class Shape {
        abstract int area(int i);
    }

    static class Square extends Shape {
        int area(int i) {
            return i * i;
        }
    }

    /** Never named by type: loaded by {@link #makeTri} only, mid-loop. */
    public static class Tri extends Shape {
        public Tri() {}

        int area(int i) {
            return i * 3 + 1;
        }
    }

    static Shape current = new Square();

    static Shape makeTri() {
        try {
            return (Shape) Class.forName("J27LoopResidencyRows$Tri").getDeclaredConstructor().newInstance();
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    static long chaLoop(int n, int switchAt) {
        long a = 0;
        int b = 1, c = 7;
        for (int i = 0; i < n; i++) {
            Shape s = current;
            a += s.area(i);
            b = b * 31 + i;
            c ^= b + (int) a;
            if (i == switchAt) current = makeTri();
        }
        return a * 31 + b * 7 + c;
    }

    static long allocLoop(int n) {
        Object[] keep = new Object[16];
        for (int j = 0; j < 16; j++) keep[j] = new int[j];
        int a = 0, b = 1;
        long s = 0;
        for (int i = 0; i < n; i++) {
            keep[i & 15] = new int[(i * 7) & 31];
            int[] got = (int[]) keep[(i * 5) & 15];
            a += got.length;
            b = b * 33 ^ a;
            s += a ^ b;
        }
        return s + a + b;
    }

    static long warm() {
        long h = 0;
        for (int r = 0; r < 20_000; r++) {
            int n = 64 + (r & 7);
            h = h * 31 + pressureInt(n);
            h = h * 31 + cellsLong(n, r, r ^ 5);
            h = h * 31 + callLoop(n);
            h = h * 31 + throwInside(n, n + r % 3).length();
            h = h * 31 + chaLoop(n, -1);
            h = h * 31 + allocLoop(n);
        }
        return h;
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    public static void main(String[] args) {
        fact("warm", warm());
        for (int n : new int[] {0, 1, 2, 5, 10, 100, 1000, 100_000, 10_000_000}) {
            fact("pressureInt-" + n, pressureInt(n));
        }
        long[][] cells = {{0, 5, 3}, {1000, 5, 3}, {1_000_000, 123_456_789, -77}, {10_000_000, -1, 1L << 40}};
        for (long[] r : cells) {
            fact("cellsLong-" + r[0] + "-" + r[1] + "-" + r[2], cellsLong((int) r[0], r[1], r[2]));
        }
        for (int n : new int[] {0, 9, 10, 1000, 100_000}) fact("callLoop-" + n, callLoop(n));
        int[][] throwing = {{5000, 2500}, {5000, 6000}, {10, 3}};
        for (int[] r : throwing) fact("throwInside-" + r[0] + "-" + r[1], throwInside(r[0], r[1]));
        fact("chaLoop-switch-100000-50000", chaLoop(100_000, 50_000));
        fact("chaLoop-tri-1000", chaLoop(1000, -1));
        current = new Square();
        fact("chaLoop-square-1000", chaLoop(1000, -1));
        for (int n : new int[] {0, 16, 3_000_000}) fact("allocLoop-" + n, allocLoop(n));
        if (Boolean.getBoolean("j27.time")) {
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                int p = pressureInt(100_000_000);
                long t1 = System.nanoTime();
                long q = cellsLong(100_000_000, 5, 3);
                long t2 = System.nanoTime();
                System.out.println("time pressureInt-1e8 " + (t1 - t0) + " " + (p & 1));
                System.out.println("time cellsLong-1e8 " + (t2 - t1) + " " + (q & 1));
            }
        }
    }
}
