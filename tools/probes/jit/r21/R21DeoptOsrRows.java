// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane keepset: a differential census of deopt and OSR
// state. Every row runs a hot loop (OSR-compiled and, after the warm-up
// rounds, method-entry compiled) whose live state at the moment of a forced
// exit is a mix of the shapes the frame-state / OSR-entry code has to get
// right, then prints facts derived ONLY from that state:
//
//   cha        a CHA-devirtualised call inside a long loop; a subclass is
//              loaded from inside the loop at iteration K, which invalidates
//              the running body (its own frame is on the stack: the
//              dependency is broken under it)
//   rare       a branch never taken during warm-up and taken once at the end
//              (an uncommon trap), with long / double / float / ref locals
//              live across it, including NaN payloads, -0.0 and MIN_VALUE
//   pairs      long / double locals in adjacent two-slot pairs between ints,
//              the loop counter itself a long, OSR-entered mid-loop
//   reuse      one local slot reused for a long in one block scope and for a
//              double then a ref in the next (javac reuses slots): OSR entry
//              and a trap in the second scope
//   monitor    an OSR-entered loop inside `synchronized (lock)`, nested twice
//              on the same lock, trapping on a rare branch while the lock is
//              held; then a second thread must be able to take the lock
//   syncmeth   a synchronized method trapping inside its loop
//   nested     a trap in the innermost of three inlinable scopes, each with
//              its own long / double state
//   catch      an exception thrown and caught inside an OSR loop at
//              iteration K, the loop continuing; and one escaping the loop to
//              the caller with its message
//   phi        a value whose static type differs between the two arms that
//              join (Integer vs String into an Object local), read after a
//              trap
//
// Each row prints `fact <row> <name> <value>` lines. Every one is pure
// arithmetic on fixed inputs, so HotSpot 25, every CratonVM arm and --nojit
// must print exactly the same text (no timing rows).
//
// Run:
//   javac -d out R21DeoptOsrRows.java
//   java -Xmx256m -cp out R21DeoptOsrRows                      # HotSpot 25
//   cratonvm -Xmx256m -cp out R21DeoptOsrRows                  # default (--jdk-only)
//   cratonvm --compatible -Xmx256m -cp out R21DeoptOsrRows
//   cratonvm --nojit -Xmx256m -cp out R21DeoptOsrRows          # facts only
// Engagement: CRATONVM_DBG_DEOPT=1 / CRATONVM_DBG_JITC=1 should show OSR
// entries and deopts for the rows (`[cratonvm-deopt] ...`). Diff the fact lines
// against HotSpot; any difference is a defect.

public class R21DeoptOsrRows {
    static final int WARM = 6;
    static final long N = 400_000L;

    static volatile Object sinkObj;
    static volatile long sinkLong;

    static void fact(String row, String name, Object value) {
        System.out.println("fact " + row + " " + name + " " + value);
    }

    // ── cha ──────────────────────────────────────────────────────────
    abstract static class Shape {
        abstract long weight(long i);
    }

    static final class ShapeA extends Shape {
        long weight(long i) {
            return (i & 7) + 1;
        }
    }

    static final class ShapeB extends Shape {
        long weight(long i) {
            return 100;
        }
    }

    static Shape loadB() {
        try {
            return (Shape) Class.forName("R21DeoptOsrRows$ShapeB").getDeclaredConstructor().newInstance();
        } catch (ReflectiveOperationException e) {
            throw new RuntimeException(e);
        }
    }

    static long chaLoop(Shape s, long n, long loadAt) {
        long acc = 0;
        double d = 0.5;
        Shape cur = s;
        for (long i = 0; i < n; i++) {
            acc += cur.weight(i);
            d = d * 1.0000001 + 0.25;
            if (i == loadAt) {
                cur = loadB();
            }
        }
        return acc ^ Double.doubleToRawLongBits(d);
    }

    static void cha() {
        Shape a = new ShapeA();
        long warm = 0;
        for (int r = 0; r < WARM; r++) {
            warm += chaLoop(a, N, -1);
        }
        fact("cha", "warm", warm);
        fact("cha", "after-load", chaLoop(a, N, N / 2));
        fact("cha", "second-impl", chaLoop(new ShapeB(), 1000, -1));
    }

    // ── rare ─────────────────────────────────────────────────────────
    static String rareLoop(long n, long trapAt) {
        long l = Long.MIN_VALUE;
        double dn = Double.longBitsToDouble(0x7ff8_dead_beef_0001L);
        double nz = -0.0;
        float fn = Float.intBitsToFloat(0x7fc1_2345);
        int k = 0;
        Object ref = "start";
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += i ^ l;
            k = k * 31 + (int) i;
            if (i == trapAt) {
                ref = new StringBuilder("trap@").append(i);
                acc += 17;
            }
        }
        return acc + " " + k + " " + l + " " + Long.toHexString(Double.doubleToRawLongBits(dn))
                + " " + Long.toHexString(Double.doubleToRawLongBits(nz)) + " "
                + Integer.toHexString(Float.floatToRawIntBits(fn)) + " " + ref;
    }

    static void rare() {
        String s = "";
        for (int r = 0; r < WARM; r++) {
            s = rareLoop(N, -1);
        }
        fact("rare", "warm", s);
        fact("rare", "trapped", rareLoop(N, N - 3));
    }

    // ── pairs ────────────────────────────────────────────────────────
    static String pairsLoop(long n, int bump) {
        int a = 3;
        long b = 0x1234_5678_9abc_def0L;
        int c = 5;
        double d = 1.5;
        long e = -7;
        double f = -2.25;
        int g = 11;
        for (long i = 0; i < n; i++) {
            a += (int) (i & 3);
            b = b * 6364136223846793005L + 1442695040888963407L;
            c ^= a;
            d = d * 0.999999 + 1.0;
            e -= i;
            f = f + 0.125;
            g += c & 1;
            if (i == bump) {
                f = Double.NEGATIVE_INFINITY;
            }
        }
        return a + " " + b + " " + c + " " + Double.doubleToRawLongBits(d) + " " + e + " "
                + Double.doubleToRawLongBits(f) + " " + g;
    }

    static void pairs() {
        String s = "";
        for (int r = 0; r < WARM; r++) {
            s = pairsLoop(N, -1);
        }
        fact("pairs", "warm", s);
        fact("pairs", "bumped", pairsLoop(N, (int) (N - 5)));
    }

    // ── reuse ────────────────────────────────────────────────────────
    static String reuseLoop(long n, long trapAt) {
        long first;
        {
            long x = 1;
            for (long i = 0; i < n; i++) {
                x = x * 3 + i;
            }
            first = x;
        }
        String out;
        {
            double y = 0.75;
            Object z = null;
            for (long i = 0; i < n; i++) {
                y = y * 1.0000003 - 0.5;
                if (i == trapAt) {
                    z = Long.valueOf(i);
                }
            }
            out = Double.doubleToRawLongBits(y) + " " + z;
        }
        return first + " " + out;
    }

    static void reuse() {
        String s = "";
        for (int r = 0; r < WARM; r++) {
            s = reuseLoop(N, -1);
        }
        fact("reuse", "warm", s);
        fact("reuse", "trapped", reuseLoop(N, N / 3));
    }

    // ── monitor ──────────────────────────────────────────────────────
    static final Object LOCK = new Object();

    static long monitorLoop(long n, long trapAt) {
        long acc = 0;
        double d = 3.0;
        synchronized (LOCK) {
            synchronized (LOCK) {
                for (long i = 0; i < n; i++) {
                    acc += i * 3;
                    d = d * 0.5 + 1.0;
                    if (i == trapAt) {
                        acc += Thread.holdsLock(LOCK) ? 1000 : -1000;
                    }
                }
            }
            acc += Thread.holdsLock(LOCK) ? 1 : -1;
        }
        acc += Thread.holdsLock(LOCK) ? -10 : 10;
        return acc ^ Double.doubleToRawLongBits(d);
    }

    static synchronized long syncMethodLoop(long n, long trapAt) {
        long acc = 7;
        for (long i = 0; i < n; i++) {
            acc = acc * 5 + i;
            if (i == trapAt) {
                acc += Thread.holdsLock(R21DeoptOsrRows.class) ? 99 : -99;
            }
        }
        return acc;
    }

    static boolean otherThreadTakes(Object lock) throws InterruptedException {
        boolean[] took = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (lock) {
                took[0] = true;
            }
        });
        t.start();
        t.join(10_000);
        return took[0] && !t.isAlive();
    }

    static void monitor() throws InterruptedException {
        long w = 0;
        for (int r = 0; r < WARM; r++) {
            w += monitorLoop(N, -1);
        }
        fact("monitor", "warm", w);
        fact("monitor", "trapped", monitorLoop(N, N - 2));
        fact("monitor", "held-after", Thread.holdsLock(LOCK));
        fact("monitor", "other-thread-takes", otherThreadTakes(LOCK));
        long m = 0;
        for (int r = 0; r < WARM; r++) {
            m += syncMethodLoop(N, -1);
        }
        fact("syncmeth", "warm", m);
        fact("syncmeth", "trapped", syncMethodLoop(N, N - 4));
        fact("syncmeth", "other-thread-takes", otherThreadTakes(R21DeoptOsrRows.class));
    }

    // ── nested ───────────────────────────────────────────────────────
    static long inner(long i, long trapAt) {
        long q = i * 7;
        double r = q * 0.5;
        if (i == trapAt) {
            sinkObj = new int[] {(int) q};
            q += 3;
        }
        return q + (long) r;
    }

    static long middle(long i, long trapAt) {
        double m = i + 0.25;
        long v = inner(i, trapAt);
        return v + (long) (m * 2);
    }

    static long outer(long n, long trapAt) {
        long acc = 0;
        double od = 0.0;
        for (long i = 0; i < n; i++) {
            acc += middle(i, trapAt);
            od += 0.5;
        }
        return acc ^ (long) od;
    }

    static void nested() {
        long w = 0;
        for (int r = 0; r < WARM; r++) {
            w += outer(N, -1);
        }
        fact("nested", "warm", w);
        fact("nested", "trapped", outer(N, N - 7));
    }

    // ── catch ────────────────────────────────────────────────────────
    static long catchLoop(long n, long throwAt) {
        long acc = 0;
        double d = 1.0;
        int caught = 0;
        for (long i = 0; i < n; i++) {
            try {
                if (i == throwAt) {
                    throw new IllegalStateException("at " + i);
                }
                acc += i;
            } catch (IllegalStateException e) {
                caught += e.getMessage().length();
                d = -d;
            }
        }
        return acc * 31 + caught + Double.doubleToRawLongBits(d);
    }

    static long escapeLoop(long n, long throwAt) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc += i & 15;
            if (i == throwAt) {
                throw new ArithmeticException("escape " + acc);
            }
        }
        return acc;
    }

    static void catchRows() {
        long w = 0;
        for (int r = 0; r < WARM; r++) {
            w += catchLoop(N, -1);
        }
        fact("catch", "warm", w);
        fact("catch", "caught", catchLoop(N, N / 2));
        long e = 0;
        for (int r = 0; r < WARM; r++) {
            e += escapeLoop(N, -1);
        }
        fact("catch", "escape-warm", e);
        try {
            escapeLoop(N, N - 9);
            fact("catch", "escape", "none");
        } catch (ArithmeticException ex) {
            fact("catch", "escape", ex.getMessage());
        }
    }

    // ── phi ──────────────────────────────────────────────────────────
    static String phiLoop(long n, long flipAt) {
        Object o = Integer.valueOf(1);
        long acc = 0;
        for (long i = 0; i < n; i++) {
            if (i == flipAt) {
                o = "flipped" + i;
            } else if (o instanceof Integer) {
                acc += (Integer) o;
            }
        }
        return acc + " " + o + " " + (o instanceof String);
    }

    static void phi() {
        String s = "";
        for (int r = 0; r < WARM; r++) {
            s = phiLoop(N, -1);
        }
        fact("phi", "warm", s);
        fact("phi", "flipped", phiLoop(N, N / 4));
    }

    public static void main(String[] args) throws Exception {
        cha();
        rare();
        pairs();
        reuse();
        monitor();
        nested();
        catchRows();
        phi();
        System.out.println("done");
    }
}
