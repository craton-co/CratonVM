// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * j27 wave 4 (lane REMAT): fact rows for the split-reload rematerialisation of
 * {@code x + c} (CRATONVM_JIT_IR_LS_REMAT_ADD, opt-in), page
 * docs/feature-designs/jit-ir-optimizations.md (item 2).
 *
 * Loop bodies with more live int / long values than the five-register GP file,
 * two of them {@code x + c} / {@code x - c} values read early and again late
 * (the reload the switch recomputes); int and long adds that wrap at MAX / MIN
 * live across calls; a long addend no disp32 holds (the MOV+ADD form); an
 * exception thrown inside a loop whose handler reads carried values (a value a
 * deopt names keeps its home load). Every answer is Java int / long arithmetic,
 * so the expected lines are HotSpot's.
 *
 * j27 wave 9 (lane W9EXC): run the HotSpot reference with -XX:-OmitStackTraceInFastThrow.
 * Under default `java`, C2's stackless fast throw (null message) makes `warm` (it sums message lengths) and the throwInside-* rows
 * timing-dependent (measured: ...775531 / ...779616 / ...491 vs ...496 without it). CratonVM's fast throw is opt-in, so its
 * default answers equal `java -XX:-OmitStackTraceInFastThrow`, which are the lines below.
 * java J27RematRows               facts only (deterministic)
 * java -Dj27.time=1 J27RematRows  also "time <row> <ns>" lines
 */
public class J27RematRows {
    static int idInt(int v) {
        return v;
    }

    static long idLong(long v) {
        return v;
    }

    static int pressureInt(int n, int base) {
        int a = 1, b = 2, c = 3, d = 4;
        for (int i = 0; i < n; i++) {
            int x = base + i;
            int t = x + 7;
            int u = x - 3;
            a += t ^ b;
            b += u * c;
            c ^= t + d;
            d += a - u;
            a ^= t >>> 3;
            b -= u;
        }
        return a + b + c + d;
    }

    static long pressureLong(int n, long base) {
        long a = 1, b = 2, c = 3, d = 4;
        for (int i = 0; i < n; i++) {
            long x = base + i;
            long t = x + 0x10000000000L;
            long u = x - 5;
            a += t ^ b;
            b += u * c;
            c ^= t + d;
            d += a - u;
            a ^= t >>> 3;
            b -= u;
        }
        return a + b + c + d;
    }

    static int wrapInt(int x) {
        int t = x + 1;
        int u = x - Integer.MIN_VALUE;
        int r = idInt(t) ^ idInt(u);
        return r + t + u;
    }

    static long wrapLong(long x) {
        long t = x + 1L;
        long u = x - Long.MIN_VALUE;
        long w = x + 0x7FFFFFFF00000000L;
        long r = idLong(t) ^ idLong(u) ^ idLong(w);
        return r + t + u + w;
    }

    static String throwInside(int n, int at) {
        int acc = 0;
        int i = 0;
        try {
            for (; i < n; i++) {
                int t = i + 11;
                acc += t * 3;
                if (i == at) {
                    acc += t / (i - at);
                }
                acc ^= t;
            }
            return "done acc=" + acc;
        } catch (ArithmeticException e) {
            return "caught i=" + i + " acc=" + acc + " " + (e.getStackTrace().length == 0 ? "/ by zero" : e.getMessage()); // C2 fast throw is stackless
        }
    }

    static void fact(String key, Object value) {
        System.out.println("fact " + key + " " + value);
    }

    public static void main(String[] args) {
        long warm = 0;
        for (int r = 0; r < 20000; r++) {
            warm += pressureInt(r & 63, r);
            warm += pressureLong(r & 63, r * 1000003L);
            warm += wrapInt(r - 10000);
            warm += wrapLong(r * 0x9E3779B97F4A7C15L);
            warm += throwInside(r & 15, 7).length();
        }
        fact("warm", warm);
        int[] ns = {0, 1, 2, 7, 1000, 1000000};
        for (int n : ns) {
            fact("pressureInt-" + n + "-5", pressureInt(n, 5));
        }
        fact("pressureInt-1000-max500", pressureInt(1000, Integer.MAX_VALUE - 500));
        fact("pressureInt-1000-min", pressureInt(1000, Integer.MIN_VALUE));
        for (int n : ns) {
            fact("pressureLong-" + n + "-5", pressureLong(n, 5));
        }
        fact("pressureLong-1000-max500", pressureLong(1000, Long.MAX_VALUE - 500));
        fact("wrapInt-max", wrapInt(Integer.MAX_VALUE));
        fact("wrapInt-max1", wrapInt(Integer.MAX_VALUE - 1));
        fact("wrapInt-min", wrapInt(Integer.MIN_VALUE));
        fact("wrapInt-m1", wrapInt(-1));
        fact("wrapInt-0", wrapInt(0));
        fact("wrapLong-max", wrapLong(Long.MAX_VALUE));
        fact("wrapLong-min", wrapLong(Long.MIN_VALUE));
        fact("wrapLong-m1", wrapLong(-1));
        fact("wrapLong-0", wrapLong(0));
        fact("wrapLong-2p31", wrapLong(0x80000000L));
        fact("throwInside-5000-2500", throwInside(5000, 2500));
        fact("throwInside-10-3", throwInside(10, 3));
        fact("throwInside-20-100", throwInside(20, 100));
        if (Boolean.getBoolean("j27.time")) {
            long t0 = System.nanoTime();
            int s = pressureInt(100_000_000, 5);
            long t1 = System.nanoTime();
            long l = pressureLong(100_000_000, 5);
            long t2 = System.nanoTime();
            System.out.println("time pressureInt-1e8 " + (t1 - t0) + " (" + s + ")");
            System.out.println("time pressureLong-1e8 " + (t2 - t1) + " (" + l + ")");
        }
    }
}
