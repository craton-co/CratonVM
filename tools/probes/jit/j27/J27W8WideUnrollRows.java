// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * j27 wave 8 (lane W8LOOP): fact rows for the wide main/post unroll of a body
 * that cannot deopt (CRATONVM_JIT_IR_UNROLL_WIDE_PURE=1, opt-in), page
 * docs/known-issues/jit/jr21-promoted-loop-bodies-spill-and-poll-every-trip-20261026.md.
 *
 * Every kernel is a counted loop with a pure body (int / long arithmetic and
 * promoted static cells), warmed until compiled, then asked at trip counts on
 * both sides of every factor's boundary (0, 1, 3, 7, 8, 9, 15, 16, 17) and at
 * bounds where the main loop's licence `n - d < n` must refuse (near
 * Integer.MAX_VALUE and Integer.MIN_VALUE). The answers are closed forms of
 * Java int / long arithmetic, so the expected lines are HotSpot 25's.
 *
 * Expected HotSpot 25 output (deterministic), in this order:
 *   fact sumInt   0 0 3 21 28 36 105 120 136 499500 1783293664
 *   fact counter  0 3 9 21 24 27 45 48 51 3000 3000000
 *   fact other    0 0 3 7 0 8 15 0 16 0 0
 *   fact stride2  0 0 2 12 12 20 56 56 72 249500 891396832
 *   fact sumSq    0 0 5 91 140 204 1015 1240 1496 332833500 333332833333500000
 *   fact nearMax 5
 *   fact nearMin 3
 *   fact varStart -9
 *
 * java J27W8WideUnrollRows                 facts only
 * java -Dj27.time=true J27W8WideUnrollRows also "time <row> <ns>" lines
 */
public class J27W8WideUnrollRows {
    static int COUNTER;
    static int OTHER;

    static final int[] NS = {0, 1, 3, 7, 8, 9, 15, 16, 17, 1000, 1000000};

    static int sumInt(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static void twoCells(int n) {
        for (int i = 0; i < n; i++) {
            COUNTER += 3;
            OTHER ^= i;
        }
    }

    static int stride2(int n) {
        int s = 0;
        for (int i = 0; i < n; i += 2) {
            s += i;
        }
        return s;
    }

    static long sumSq(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (long) i * i;
        }
        return s;
    }

    static int countFrom(int a, int b) {
        int c = 0;
        for (int i = a; i < b; i++) {
            c++;
        }
        return c;
    }

    static int sumFrom(int a, int b) {
        int s = 0;
        for (int i = a; i < b; i++) {
            s += i;
        }
        return s;
    }

    static long warm() {
        long sink = 0;
        for (int r = 0; r < 30000; r++) {
            int n = 1 + (r % 40);
            sink += sumInt(n) + stride2(n) + sumSq(n) + countFrom(r, r + n) + sumFrom(-n, n);
            twoCells(n);
        }
        return sink + COUNTER + OTHER;
    }

    interface Row {
        String at(int n);
    }

    static void row(String name, Row r) {
        StringBuilder sb = new StringBuilder("fact ").append(name);
        for (int n : NS) {
            sb.append(' ').append(r.at(n));
        }
        System.out.println(sb);
    }

    public static void main(String[] args) {
        warm();
        row("sumInt", n -> Integer.toString(sumInt(n)));
        row("counter", n -> {
            COUNTER = 0;
            OTHER = 0;
            twoCells(n);
            return Integer.toString(COUNTER);
        });
        row("other", n -> {
            COUNTER = 0;
            OTHER = 0;
            twoCells(n);
            return Integer.toString(OTHER);
        });
        row("stride2", n -> Integer.toString(stride2(n)));
        row("sumSq", n -> Long.toString(sumSq(n)));
        System.out.println("fact nearMax " + countFrom(Integer.MAX_VALUE - 5, Integer.MAX_VALUE));
        System.out.println("fact nearMin " + countFrom(Integer.MIN_VALUE, Integer.MIN_VALUE + 3));
        System.out.println("fact varStart " + sumFrom(-5, 4));
        if (Boolean.getBoolean("j27.time")) {
            time();
        }
    }

    static void time() {
        int reps = 20000;
        long t0 = System.nanoTime();
        long sink = 0;
        for (int r = 0; r < reps; r++) {
            sink += sumInt(1000);
        }
        long t1 = System.nanoTime();
        for (int r = 0; r < reps; r++) {
            twoCells(1000);
        }
        long t2 = System.nanoTime();
        System.out.println("time sumInt1000 " + (t1 - t0) / reps);
        System.out.println("time twoCells1000 " + (t2 - t1) / reps);
        System.out.println("time sink " + (sink + COUNTER));
    }
}
