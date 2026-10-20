// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * nio21 wave 10, lane L2: divisions inside spliced callees
 * (docs/internal/fixed-bugs/r21-ir-splice-refuses-a-division-by-a-non-zero-constant-CLOSED-20261012.md,
 * proposal W9L2-3 in docs/internal/proposals/jit/jit-r21-nio-proposals-w9-L2.md).
 *
 * <ul>
 *   <li>{@code CRATONVM_JIT_IR_SPLICE_CONST_DIVISION} (default ON): a callee whose division has a
 *       constant divisor other than 0 ({@code x / 7}, {@code x % -1}, {@code x / MIN_VALUE}) is
 *       spliced with no zero-divisor guard. Rows {@code consts}, {@code zero}.</li>
 *   <li>{@code CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_FLOOR} + {@code CRATONVM_JIT_IR_SPLICE_DIV_GUARD}
 *       (default ON): the JDK bodies of {@code Math.floorDiv} / {@code floorMod} are spliced; a
 *       variable divisor keeps a guard that deopts so the interpreter throws. Rows {@code floor},
 *       {@code flip}.</li>
 *   <li>nio21 wave 12 L4, {@code CRATONVM_JIT_IR_SPLICE_SCREENS_CONST_DIVISION} (default ON): the
 *       self-recursive copy, synchronized-splice, cannot-throw and handler-range screens admit a
 *       division by a non-zero constant. Row {@code screens}.</li>
 *   <li>nio21 wave 12 L4 / wave 14 L2, {@code CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_MULTIPLY_EXACT_LONG}
 *       (default ON since wave 14, {@code =0} off): the JDK body of {@code Math.multiplyExact(JJ)J}
 *       is spliced, and {@code Math.abs(J)J}'s only NESTED inside a splice ({@code absPlus},
 *       {@code multiplyExact}); a top-level {@code Math.abs(long)} keeps the scalar op. Row
 *       {@code mulexact}; bench {@code mulexact}, {@code absl} (top level, should not move with the
 *       switch), {@code absplus} (nested).</li>
 * </ul>
 *
 * <pre>
 *   javac -d /tmp/p21 tools/probes/Nio21DivSplice.java
 *   # C2 throws a PREALLOCATED, trace-less ArithmeticException from a hot site
 *   # (OmitStackTraceInFastThrow); the reference is taken with the omission off.
 *   java -XX:-OmitStackTraceInFastThrow -cp /tmp/p21 Nio21DivSplice > hs.txt
 *   cratonvm --jdk-only   -cp /tmp/p21 Nio21DivSplice > jo.txt
 *   cratonvm --compatible -cp /tmp/p21 Nio21DivSplice > co.txt
 *   CRATONVM_JIT_IR_SPLICE_CONST_DIVISION=0 CRATONVM_JIT_IR_SPLICE_DIV_GUARD=0 \
 *       cratonvm --jdk-only -cp /tmp/p21 Nio21DivSplice > jo0.txt       # hs = jo = co = jo0
 *   CRATONVM_JIT_IR_SPLICE_SCREENS_CONST_DIVISION=0 cratonvm --jdk-only -cp /tmp/p21 Nio21DivSplice > jos.txt
 *   CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_MULTIPLY_EXACT_LONG=0 cratonvm --jdk-only -cp /tmp/p21 Nio21DivSplice > jom.txt
 *   CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_MULTIPLY_EXACT_LONG=0 cratonvm --compatible -cp /tmp/p21 Nio21DivSplice > com.txt
 *   # hs = jos = jom = com
 *   java     -cp /tmp/p21 -Dnio21.bench=1 Nio21DivSplice                # interleave, 5 runs each
 *   cratonvm -cp /tmp/p21 -Dnio21.bench=1 Nio21DivSplice
 *   CRATONVM_JIT_IR_SPLICE_CONST_DIVISION=0 CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_FLOOR=0 \
 *       cratonvm -cp /tmp/p21 -Dnio21.bench=1 Nio21DivSplice
 * </pre>
 *
 * {@code -Dnio21.rows=a,b} selects rows (default all). Conformance prints answers only; the bench
 * ({@code -Dnio21.bench=1} or {@code true}) prints one {@code <row> <N> ns/op} line per row, the
 * median of five timed passes after three warm-up passes ({@code -Dnio21.iters=N}, default 200000).
 */
public class Nio21DivSplice {
    static final String ROWS = System.getProperty("nio21.rows", "all");
    static final int WARM = 30_000;

    static final int[] XI = {
        Integer.MIN_VALUE, Integer.MIN_VALUE + 1, -1_000_001, -86_401, -8, -7, -6, -1, 0, 1, 6, 7, 8,
        999, 1000, 86_400, 1_000_001, Integer.MAX_VALUE - 1, Integer.MAX_VALUE
    };
    static final long[] XL = {
        Long.MIN_VALUE, Long.MIN_VALUE + 1, -1_790_000_000_123L, -86_401L, -8, -7, -1, 0, 1, 7, 8,
        1000, 1_790_000_000_123L, Long.MAX_VALUE - 1, Long.MAX_VALUE
    };

    static boolean on(String r) {
        return ROWS.equals("all") || Arrays.asList(ROWS.split(",")).contains(r);
    }

    static String ex(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        String top = st.length == 0 ? "<none>" : st[0].getClassName() + "." + st[0].getMethodName();
        return t.getClass().getName() + ": " + t.getMessage() + " @ " + top;
    }

    // ---- small callees, each a splice candidate of the compiled callers below ---------------

    static int dI1(int x) { return x / 1; }            // iconst_1
    static int dIm1(int x) { return x / -1; }          // iconst_m1
    static int dI7(int x) { return x / 7; }            // bipush
    static int dIm7(int x) { return x / -7; }          // bipush
    static int dIk(int x) { return x / 1000; }         // sipush
    static int dImin(int x) { return x / Integer.MIN_VALUE; } // ldc
    static int rI1(int x) { return x % 1; }
    static int rIm1(int x) { return x % -1; }
    static int rI7(int x) { return x % 7; }
    static int rIm7(int x) { return x % -7; }
    static int rImin(int x) { return x % Integer.MIN_VALUE; }
    static int dI0(int x) { return x / 0; }            // iconst_0: refused, throws
    static int rI0(int x) { return x % 0; }

    static long dL1(long x) { return x / 1L; }         // lconst_1
    static long dLm1(long x) { return x / -1L; }       // ldc2_w
    static long dL7(long x) { return x / 7L; }
    static long dLm7(long x) { return x / -7L; }
    static long dLmilli(long x) { return x / 1_000_000L; } // the Duration.toMillis shape
    static long dLmin(long x) { return x / Long.MIN_VALUE; }
    static long rL1(long x) { return x % 1L; }
    static long rLm1(long x) { return x % -1L; }
    static long rL7(long x) { return x % 7L; }
    static long rLm7(long x) { return x % -7L; }
    static long rLmin(long x) { return x % Long.MIN_VALUE; }
    static long dL0(long x) { return x / 0L; }         // lconst_0: refused, throws
    static long rL0(long x) { return x % 0L; }

    static long intOps(int x) {
        return (long) dI1(x) + dIm1(x) + dI7(x) + dIm7(x) + dIk(x) + dImin(x) + rI1(x) + rIm1(x)
                + rI7(x) + rIm7(x) + rImin(x);
    }

    static long longOps(long x) {
        return dL1(x) + dLm1(x) + dL7(x) + dLm7(x) + dLmilli(x) + dLmin(x) + rL1(x) + rLm1(x)
                + rL7(x) + rLm7(x) + rLmin(x);
    }

    // floorDiv / floorMod with constant divisors at the call (the java.time shape) ...
    static long floorConstOps(int x, long l) {
        return Math.floorDiv(x, 1000) + Math.floorMod(x, 86_400) + Math.floorDiv(x, -7)
                + Math.floorMod(x, -7) + Math.floorDiv(x, -1) + Math.floorMod(x, -1)
                + Math.floorDiv(x, Integer.MIN_VALUE) + Math.floorMod(x, Integer.MIN_VALUE)
                + Math.floorDiv(l, 1000L) + Math.floorMod(l, 86_400L) + Math.floorDiv(l, -1L)
                + Math.floorMod(l, -1L) + Math.floorDiv(l, Long.MIN_VALUE) + Math.floorMod(l, Long.MIN_VALUE)
                + Math.floorDiv(l, 1000) + Math.floorMod(l, 60) + Math.floorDiv(l, -7) + Math.floorMod(l, -7);
    }

    // ... and variable ones.
    static int fdI(int x, int y) { return Math.floorDiv(x, y); }
    static int fmI(int x, int y) { return Math.floorMod(x, y); }
    static long fdL(long x, long y) { return Math.floorDiv(x, y); }
    static long fmL(long x, long y) { return Math.floorMod(x, y); }
    static long fdLI(long x, int y) { return Math.floorDiv(x, y); }
    static int fmLI(long x, int y) { return Math.floorMod(x, y); }

    static long floorVarOps(int x, int y, long l, long m) {
        return (long) fdI(x, y) + fmI(x, y) + fdL(l, m) + fmL(l, m) + fdLI(l, y) + fmLI(l, y);
    }

    // ---- wave 12: the byte screens and multiplyExact(long, long) ---------------------------

    /** A self-recursive body dividing by a constant: the trap-free self-copy route. */
    static int digits(int n) {
        return n < 10 && n > -10 ? 1 : 1 + digits(n / 10);
    }

    static long sumDigits(long n) {
        return n == 0 ? 0 : (n % 10L) + sumDigits(n / 10L);
    }

    static final class Counter {
        private int count;

        /** A synchronized body dividing by constants: the synchronized-splice scan. */
        synchronized int step(int x) {
            count += x % 7;
            return count / 3 + x / -1;
        }

        synchronized int get() {
            return count;
        }
    }

    static int stepCaller(Counter c, int x) {
        return c.step(x);
    }

    /** A static leaf that cannot throw, dividing by an immediate. */
    static int scale(int x) {
        return x / 16 + x % 5;
    }

    static long mulOps(long a, long b, int c) {
        return Math.multiplyExact(a, b) + Math.multiplyExact(a, c) + Math.abs(a - b);
    }

    static long mulJJ(long a, long b) {
        return Math.multiplyExact(a, b);
    }

    static long mulJI(long a, int b) {
        return Math.multiplyExact(a, b);
    }

    static long absL(long a) {
        return Math.abs(a);
    }

    /** A spliceable leaf whose {@code Math.abs(long)} is a NESTED site (wave 14). */
    static long absPlus(long a) {
        return Math.abs(a) + 1;
    }

    static void screens() {
        long sum = 0;
        Counter c = new Counter();
        for (int i = 0; i < WARM; i++) {
            int x = i * 7_919 - 50_000;
            sum += digits(x) + sumDigits((long) x * 1_000_003L) + stepCaller(c, x) + scale(x);
        }
        System.out.println("screens sum " + sum + " count " + c.get());
        for (int x : XI) {
            System.out.println("screens int " + x + ": " + digits(x) + " " + sumDigits(x) + " " + scale(x));
        }
        for (long x : XL) {
            System.out.println("screens long " + x + ": " + sumDigits(x));
        }
    }

    /** Warm the multiplyExact callers without overflow, then overflow the SAME compiled sites. */
    static void mulexact() {
        long sum = 0;
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < WARM; i++) {
                long a = (long) i * 1_000_003L - 7_000_000_000L;
                sum += mulOps(a, i - 15_000, i & 1023) + absL(a) + absPlus(a ^ i);
            }
            int thrown = 0;
            String last = "";
            for (int k = 0; k < 16; k++) {
                try {
                    sum += mulJJ(Long.MAX_VALUE - k, 2 + k % 2);
                } catch (ArithmeticException e) {
                    thrown++;
                    last = ex(e);
                }
                try {
                    sum += mulJI(Long.MIN_VALUE + k % 2, -1);
                } catch (ArithmeticException e) {
                    thrown++;
                    last = last + " | " + ex(e);
                }
            }
            System.out.println("mulexact round " + round + " thrown=" + thrown + " " + last);
        }
        System.out.println("mulexact sum " + sum);
        long[] as = {Long.MIN_VALUE, -3_037_000_500L, -1, 0, 1, 3_037_000_499L, 3_037_000_500L, Long.MAX_VALUE};
        long[] bs = {Long.MIN_VALUE, -3_037_000_500L, -2, -1, 0, 1, 2, 3_037_000_499L, Long.MAX_VALUE};
        for (long a : as) {
            StringBuilder sb = new StringBuilder("mulexact " + a + ":");
            for (long bb : bs) {
                try {
                    sb.append(' ').append(mulJJ(a, bb));
                } catch (ArithmeticException e) {
                    sb.append(" !").append(e.getMessage());
                }
            }
            try {
                sb.append(" | ").append(mulJI(a, -1));
            } catch (ArithmeticException e) {
                sb.append(" | !").append(e.getMessage());
            }
            sb.append(' ').append(absL(a)).append(' ').append(absPlus(a));
            System.out.println(sb);
        }
    }

    // ---- conformance -----------------------------------------------------------------------

    static void consts() {
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += intOps(i * 7_919 - 50_000) + longOps((long) i * 1_000_003L - 7_000_000_000L);
        }
        System.out.println("consts sum " + sum);
        for (int x : XI) {
            System.out.println("consts int " + x + ": " + dI1(x) + " " + dIm1(x) + " " + dI7(x) + " "
                    + dIm7(x) + " " + dIk(x) + " " + dImin(x) + " | " + rI1(x) + " " + rIm1(x) + " "
                    + rI7(x) + " " + rIm7(x) + " " + rImin(x));
        }
        for (long x : XL) {
            System.out.println("consts long " + x + ": " + dL1(x) + " " + dLm1(x) + " " + dL7(x) + " "
                    + dLm7(x) + " " + dLmilli(x) + " " + dLmin(x) + " | " + rL1(x) + " " + rLm1(x)
                    + " " + rL7(x) + " " + rLm7(x) + " " + rLmin(x));
        }
    }

    interface Thrower {
        long run();
    }

    static void tryRow(String row, String name, Thrower t) {
        try {
            System.out.println(row + " " + name + " = " + t.run());
        } catch (ArithmeticException e) {
            System.out.println(row + " " + name + " " + ex(e));
        }
    }

    static int zeroCaller(int x) {
        int n = 0;
        try {
            n += dI0(x);
        } catch (ArithmeticException e) {
            n += 1;
        }
        try {
            n += rI0(x);
        } catch (ArithmeticException e) {
            n += 2;
        }
        try {
            n += (int) dL0(x);
        } catch (ArithmeticException e) {
            n += 4;
        }
        try {
            n += (int) rL0(x);
        } catch (ArithmeticException e) {
            n += 8;
        }
        return n;
    }

    static void zero() {
        long sum = 0;
        for (int i = 0; i < WARM; i++) sum += zeroCaller(i);
        System.out.println("zero sum " + sum);
        tryRow("zero", "dI0", () -> dI0(5));
        tryRow("zero", "rI0", () -> rI0(5));
        tryRow("zero", "dL0", () -> dL0(5));
        tryRow("zero", "rL0", () -> rL0(5));
    }

    static void floor() {
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += floorConstOps(i * 7_919 - 50_000, (long) i * 1_000_003L - 7_000_000_000L);
        }
        System.out.println("floor sum " + sum);
        for (int x : XI) {
            System.out.println("floor int " + x + ": " + floorConstOps(x, x));
        }
        for (long x : XL) {
            System.out.println("floor long " + x + ": " + floorConstOps((int) x, x));
        }
        int[] ys = {Integer.MIN_VALUE, -86_400, -7, -1, 1, 7, 1000, Integer.MAX_VALUE};
        long[] ms = {Long.MIN_VALUE, -1000, -7, -1, 1, 7, 86_400, Long.MAX_VALUE};
        for (int x : XI) {
            StringBuilder sb = new StringBuilder("floor var " + x + ":");
            for (int k = 0; k < ys.length; k++) {
                long l = (long) x * 3 - 1;
                sb.append(' ').append(fdI(x, ys[k])).append('/').append(fmI(x, ys[k])).append('/')
                        .append(fdL(l, ms[k])).append('/').append(fmL(l, ms[k])).append('/')
                        .append(fdLI(l, ys[k])).append('/').append(fmLI(l, ys[k]));
            }
            System.out.println(sb);
        }
    }

    /** Warm every variable-divisor caller on non-zero divisors only, then divide by zero at the SAME compiled site. */
    static void flip() {
        long sum = 0;
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < WARM; i++) {
                int y = (i % 13) - 6;
                if (y == 0) y = 3;
                sum += floorVarOps(i - 15_000, y, (long) i * 977L - 9_000_000L, y * 1_000_003L);
            }
            int thrown = 0;
            String last = "";
            for (int k = 0; k < 16; k++) {
                int y = k % 2;
                try {
                    sum += fdI(k, y);
                } catch (ArithmeticException e) {
                    thrown++;
                    last = ex(e);
                }
                try {
                    sum += fmL(k, y);
                } catch (ArithmeticException e) {
                    thrown++;
                    last = last + " | " + ex(e);
                }
                try {
                    sum += fdLI(k, y);
                } catch (ArithmeticException e) {
                    thrown++;
                    last = last + " | " + ex(e);
                }
                try {
                    sum += fmLI(k, y);
                } catch (ArithmeticException e) {
                    thrown++;
                    last = last + " | " + ex(e);
                }
            }
            System.out.println("flip round " + round + " thrown=" + thrown + " " + last);
            // The overflow corner through the same warmed sites.
            System.out.println("flip min " + fdI(Integer.MIN_VALUE, -1) + " " + fmI(Integer.MIN_VALUE, -1) + " "
                    + fdL(Long.MIN_VALUE, -1L) + " " + fmL(Long.MIN_VALUE, -1L) + " "
                    + fdLI(Long.MIN_VALUE, -1) + " " + fmLI(Long.MIN_VALUE, -1));
        }
        System.out.println("flip sum " + sum);
        tryRow("flip", "fdI0", () -> fdI(1, 0));
        tryRow("flip", "fmI0", () -> fmI(-1, 0));
        tryRow("flip", "fdL0", () -> fdL(1, 0));
        tryRow("flip", "fmL0", () -> fmL(-1, 0));
    }

    // ---- bench -----------------------------------------------------------------------------

    interface Op {
        long run(int i);
    }

    static final List<String> TIMES = new ArrayList<>();

    static void bench(String name, int n, Op op) {
        if (!on(name)) return;
        long sink = 0;
        for (int w = 0; w < 3; w++) for (int i = 0; i < n; i++) sink += op.run(i);
        long[] t = new long[5];
        for (int r = 0; r < t.length; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) sink += op.run(i);
            t[r] = System.nanoTime() - t0;
        }
        Arrays.sort(t);
        TIMES.add(name + " " + (t[2] / n) + " ns/op" + (sink == 42 ? " " : ""));
    }

    public static void main(String[] args) {
        String b = System.getProperty("nio21.bench", "");
        if (b.equals("1") || b.equalsIgnoreCase("true")) {
            int n = Integer.getInteger("nio21.iters", 200_000);
            bench("divconst", n, i -> intOps(i - 100_000) + longOps((long) i * 1_000_003L));
            bench("fdconst", n, i -> floorConstOps(i - 100_000, (long) i * 1_000_003L));
            bench("fdvar", n, i -> floorVarOps(i - 100_000, (i & 15) + 1, (long) i * 977L, (i & 7) + 2));
            bench("mulexact", n, i -> mulOps((long) i * 1_000_003L, i - 100_000, i & 1023));
            bench("absl", n, i -> absL((long) i * 1_000_003L - 100_000_000_000L));
            bench("absplus", n, i -> absPlus((long) i * 1_000_003L - 100_000_000_000L));
            for (String t : TIMES) System.out.println(t);
            return;
        }
        if (on("flip")) flip(); // first: nothing has divided by zero at a compiled site before it
        if (on("consts")) consts();
        if (on("zero")) zero();
        if (on("floor")) floor();
        if (on("screens")) screens();
        if (on("mulexact")) mulexact();
    }
}
