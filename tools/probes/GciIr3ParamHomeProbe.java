// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gci i3/ir (2026-10-12): soundness of the optimizing tier's opt-in
 * {@code CRATONVM_JIT_IR_REF_PARAM_HOME_ZERO}, which zeroes a reference
 * parameter's prologue word right after the parameter is copied into its own
 * frame home ({@code jit/src/ir_lower.rs}, {@code ref_param_home_zero_candidates}).
 *
 * <p>Every method below takes reference parameters at ordinal 1 or later and
 * READS them after GC points (allocation churn, and in two judged rounds
 * {@code System.gc()}): across calls, in a loop, in an exception handler after
 * a callee threw, in a {@code synchronized} instance method, after a
 * {@code long} / {@code double} parameter, as varargs, passed on, and through
 * recursion; plus a parameter that is nulled and reassigned and one that is
 * nulled while another stays live. A wrongly zeroed word shows as a
 * {@code NullPointerException} or a different sum.
 *
 * <p>Stdout is deterministic: eleven {@code name=value} lines and
 * {@code PROBE-OK}, equal to HotSpot 25 ({@code java -XX:+UseSerialGC -Xmx64m
 * GciIr3ParamHomeProbe}). Diagnostics go to stderr.
 */
public final class GciIr3ParamHomeProbe {
    static int churnN = 1;
    static boolean gcs;
    static long sink;

    static final class Box {
        final int v;
        final String s;

        Box(int v, String s) {
            this.v = v;
            this.s = s;
        }
    }

    /** Allocates {@code churnN} small arrays; collects in the judged GC rounds. */
    static int churn() {
        int s = 0;
        for (int i = 0; i < churnN; i++) {
            byte[] b = new byte[256];
            b[i & 255] = (byte) i;
            s += b[(i * 7) & 255];
        }
        if (gcs) {
            System.gc();
        }
        return s;
    }

    static int thrower(int v) {
        if (v >= 0) {
            throw new IllegalStateException("boom" + (v & 7));
        }
        return v;
    }

    static int afterLong(long pad, Box a, double d, Box b) {
        int r = (int) pad + a.v;
        r += churn();
        r += (int) d + b.v;
        r += churn();
        return r + a.s.length() + b.s.length();
    }

    static final class Worker {
        final int base;

        Worker(int base) {
            this.base = base;
        }

        int liveAcross(Box a, String b) {
            int r = a.v;
            r += churn();
            r += a.v + b.length();
            r += churn();
            return r + b.charAt(0) + a.s.length() + base;
        }

        int reassigned(Box a) {
            int r = a.v;
            a = null;
            r += churn();
            a = new Box(r + 1, "re");
            r += churn();
            return a.v + r + a.s.length();
        }

        int nulledThenOther(Box a, Box b) {
            int r = a.v;
            a = null;
            r += churn();
            return r + b.v + b.s.length() + (a == null ? 1 : 0);
        }

        int inLoop(Box a, int n) {
            int r = 0;
            for (int i = 0; i < n; i++) {
                r += a.v + churn();
            }
            return r + a.s.length();
        }

        int withCatch(Box a, Box b) {
            int r = a.v;
            try {
                r += thrower(a.v);
            } catch (IllegalStateException e) {
                r += churn();
                r += b.v + a.s.length() + e.getMessage().length();
            }
            return r;
        }

        synchronized int syncd(Box a) {
            int r = a.v;
            r += churn();
            return r + a.v + a.s.length() + base;
        }

        int varargs(Box... xs) {
            int r = churn();
            for (Box x : xs) {
                r += x.v;
            }
            r += churn();
            return r + xs.length;
        }

        int passOn(Box a) {
            int r = churn();
            return r + liveAcross(a, a.s);
        }

        int rec(Box a, int d) {
            if (d == 0) {
                return a.v;
            }
            int r = churn();
            return r + rec(a, d - 1) + a.v;
        }
    }

    static final String[] NAMES = {
        "liveAcross", "afterLong", "reassigned", "nulledThenOther", "inLoop", "withCatch",
        "syncd", "varargs", "passOn", "rec", "afterLongNeg",
    };

    static long[] runAll(Worker w, int i) {
        Box a = new Box(i, "alpha" + (i & 3));
        Box b = new Box(i * 3 + 1, "beta");
        long[] out = new long[NAMES.length];
        out[0] = w.liveAcross(a, "gamma");
        out[1] = afterLong(i, a, 2.5, b);
        out[2] = w.reassigned(a);
        out[3] = w.nulledThenOther(a, b);
        out[4] = w.inLoop(a, 3);
        out[5] = w.withCatch(a, b);
        out[6] = w.syncd(b);
        out[7] = w.varargs(a, b, a);
        out[8] = w.passOn(b);
        out[9] = w.rec(a, 4);
        out[10] = afterLong(-i, b, 1.0, a);
        return out;
    }

    public static void main(String[] args) {
        Worker w = new Worker(7);
        long warm = 0;
        // Warm every method into the optimizing tier with almost no allocation.
        for (int round = 0; round < 2; round++) {
            for (int i = 0; i < 20_000; i++) {
                long[] o = runAll(w, i & 1023);
                warm += o[i % o.length];
            }
            if (round == 0) {
                try {
                    Thread.sleep(1000);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            }
        }
        sink += warm;
        long[] total = new long[NAMES.length];
        churnN = 2048;
        for (int round = 0; round < 40; round++) {
            gcs = round == 19 || round == 39;
            long[] o = runAll(w, 1000 + round);
            for (int k = 0; k < o.length; k++) {
                total[k] += o[k];
            }
        }
        gcs = false;
        for (int k = 0; k < NAMES.length; k++) {
            System.out.println(NAMES[k] + "=" + total[k]);
        }
        System.err.println("[probe] sink=" + sink);
        System.out.println("PROBE-OK");
    }
}
