/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

import java.util.HashMap;

/**
 * gci i15/jk (2026-10-14): JNI up-calls' arguments, against HotSpot 25. The
 * up-calls ({@code Call<T>Method}, {@code CallNonvirtual<T>Method},
 * {@code CallStatic<T>Method}, {@code NewObject}) now build their Java argument
 * list in place ({@code jni.rs} {@code jni_upcall_args}); this drives every
 * parameter type, the receiver-only and one-argument shapes, the inline /
 * spilled boundary (8 values with the receiver: 7 longs; past it: 9 doubles,
 * 10 and 12 mixed parameters, a 9-parameter constructor) through the
 * {@code A}, {@code V} and variadic forms, many rounds (so collections move the
 * object arguments between calls), and {@code GetStringUTFLengthAsLong} against
 * {@code GetStringUTFLength}. Every stdout line is HotSpot's exact answer;
 * diagnostics go to stderr. Native half:
 * {@code tools/probes/jni/GciJk15JniUpcallArgsProbe.c}.
 *
 * <p>Usage: {@code GciJk15JniUpcallArgsProbe /abs/path/gcijk15.dll [rounds]}.
 *
 * <ul>
 *   <li>{@code <form> <case>:} the first round's answer of each up-call
 *       ({@code form} is {@code A}, {@code V} or {@code VA} -- the variadic
 *       one); every later round must answer the same, counted as mismatches;
 *   <li>{@code rounds:} the round count and the mismatch count (0);
 *   <li>{@code utf-long <name>:} {@code GetStringUTFLengthAsLong} and
 *       {@code GetStringUTFLength} of LATIN1, NUL, lone-surrogate, pair and
 *       long strings;
 *   <li>{@code done}.
 * </ul>
 */
public final class GciJk15JniUpcallArgsProbe {
    static final HashMap<String, String> FIRST = new HashMap<>();
    static int mismatches;
    static int seen;

    final int base;
    final String made;

    GciJk15JniUpcallArgsProbe(int base) {
        this.base = base;
        this.made = "plain";
    }

    GciJk15JniUpcallArgsProbe(boolean z, byte b, char c, short s, int i, long j, float f,
            double d, Object o) {
        this.base = i;
        this.made = describe(z, b, c, s, i, j, f, d, o, null);
    }

    static String describe(boolean z, byte b, char c, short s, int i, long j, float f, double d,
            Object o, int[] a) {
        return "z=" + z + " b=" + b + " c=" + (int) c + " s=" + s + " i=" + i + " j=" + j
                + " f=" + f + " d=" + d + " o=" + o
                + " a=" + (a == null ? "null" : a.length + ":" + a[0] + ":" + a[a.length - 1]);
    }

    int i0() {
        return base;
    }

    int i1(int x) {
        return base + x;
    }

    String made() {
        return made;
    }

    String all(boolean z, byte b, char c, short s, int i, long j, float f, double d, Object o,
            int[] a) {
        return base + " " + describe(z, b, c, s, i, j, f, d, o, a);
    }

    static String sall(boolean z, byte b, char c, short s, int i, long j, float f, double d,
            Object o, int[] a, int x, double y) {
        return describe(z, b, c, s, i, j, f, d, o, a) + " x=" + x + " y=" + y;
    }

    long l7(long a, long b, long c, long d, long e, long f, long g) {
        return base + a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g;
    }

    double d9(double a, double b, double c, double d, double e, double f, double g, double h,
            double i) {
        return a + 2 * b + 3 * c + 4 * d + 5 * e + 6 * f + 7 * g + 8 * h + 9 * i + base;
    }

    static int s2(int a, int b) {
        return a * 100 + b;
    }

    static class Base {
        String who(int a, long b, double c, Object o) {
            return "base " + a + " " + b + " " + c + " " + o;
        }
    }

    static final class Sub extends Base {
        @Override
        String who(int a, long b, double c, Object o) {
            return "sub " + a + " " + b + " " + c + " " + o;
        }
    }

    /** Called by the native for each answer. */
    static void see(String tag, String got) {
        seen++;
        String prev = FIRST.putIfAbsent(tag, got);
        if (prev == null) {
            System.out.println(tag + ": " + got);
        } else if (!prev.equals(got)) {
            mismatches++;
            System.err.println("MISMATCH " + tag + ": " + got + " (first: " + prev + ")");
        }
    }

    static void seeJ(String tag, long v) {
        see(tag, Long.toString(v));
    }

    static void seeD(String tag, double v) {
        see(tag, Double.toString(v));
    }

    /**
     * One round of every up-call, in all three forms. {@code probe} is an
     * instance (base 5), {@code sub} a {@code Sub}, {@code o} a String,
     * {@code a} an int[].
     */
    static native void round(Object probe, Object sub, Object o, int[] a);

    static native long utfLong(String s);

    static native int utfInt(String s);

    /** IsAssignableFrom(a, b) through two Class parameters (indirect locals). */
    static native boolean assignable(Class<?> a, Class<?> b);

    /** GetSuperclass(c) == NULL. */
    static native boolean superIsNull(Class<?> c);

    static volatile Object sink;

    public static void main(String[] args) {
        System.load(args[0]);
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3000;
        GciJk15JniUpcallArgsProbe probe = new GciJk15JniUpcallArgsProbe(5);
        Sub sub = new Sub();
        String o = new String("obj");
        int[] a = new int[] {11, 12, 13};
        for (int r = 0; r < rounds; r++) {
            round(probe, sub, o, a);
            // Garbage between rounds, so collections run (and move the
            // arguments) while the round's up-calls hold them.
            for (int k = 0; k < 8; k++) {
                sink = new byte[4096];
            }
        }
        System.out.println("rounds: " + rounds + " answers=" + FIRST.size() + " mismatches="
                + mismatches + " calls-per-round=" + (seen / rounds));

        StringBuilder big = new StringBuilder();
        for (int i = 0; i < 70_001; i++) {
            big.append((char) ('a' + i % 26));
        }
        big.setCharAt(35_000, '\u00e9');
        String[][] cases = {
            {"latin1", "abc"},
            {"empty", ""},
            {"nul", "a\u0000b"},
            {"two-byte", "\u00e9\u07ff"},
            {"three-byte", "\u0800\uffff"},
            {"lone", "a\ud800b"},
            {"pair", "\ud83d\ude00"},
            {"long", big.toString()},
        };
        for (String[] c : cases) {
            System.out.println("utf-long " + c[0] + ": as-long=" + utfLong(c[1]) + " int="
                    + utfInt(c[1]));
        }

        // gci i15/jk: primitive mirrors decode to no class id; HotSpot answers
        // IsAssignableFrom by mirror identity for them.
        Class<?>[][] pairs = {
            {int.class, int.class},
            {int.class, long.class},
            {int.class, Object.class},
            {Object.class, int.class},
            {void.class, void.class},
            {String.class, Object.class},
            {Object.class, String.class},
            {int[].class, Object.class},
            {int[].class, int[].class},
            {String[].class, Object[].class},
        };
        StringBuilder as = new StringBuilder("assignable:");
        for (Class<?>[] p : pairs) {
            as.append(' ').append(p[0].getSimpleName()).append("->").append(p[1].getSimpleName())
                    .append('=').append(assignable(p[0], p[1]));
        }
        System.out.println(as);
        System.out.println("super-null: int=" + superIsNull(int.class) + " void="
                + superIsNull(void.class) + " Object=" + superIsNull(Object.class) + " String="
                + superIsNull(String.class));
        System.out.println("done");
    }
}
