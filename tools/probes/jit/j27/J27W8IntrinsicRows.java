// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * JIT j27 wave 8, lane W8INTR: Integer/Long compareUnsigned (HotSpot 25's
 * _compareUnsigned_i / _compareUnsigned_l) and Integer/Long signum, which the
 * optimizing tier expands into one three-way compare under
 * CRATONVM_JIT_IR_INT_COMPARE_EXPAND=1 (default OFF;
 * jit/src/ir/pure_expand_int_compare.rs). In --compatible all four are
 * registered Intrinsic natives (a funnel call per op otherwise).
 *
 * The row methods are called WARM times on random and edge inputs (so they
 * compile), every answer checked against an independent reference written
 * from the javadoc (an unsigned order / the sign), then the edge answers are
 * printed through the warm row methods. Run unset and with the switch =1,
 * --real-jdk and --compatible; stdout must equal HotSpot 25 byte for byte:
 *
 *   fact cmpu-int-neg1-0 1
 *   fact cmpu-int-0-neg1 -1
 *   fact cmpu-int-min-max 1
 *   fact cmpu-int-max-min -1
 *   fact cmpu-int-min-min 0
 *   fact cmpu-int-1-2 -1
 *   fact cmpu-int-neg2-neg1 -1
 *   fact cmpu-long-neg1-0 1
 *   fact cmpu-long-min-max 1
 *   fact cmpu-long-max-min -1
 *   fact cmpu-long-eq 0
 *   fact cmpu-long-hi32 1
 *   fact cmpu-long-low-vs-neg -1
 *   fact signum-int 0,1,-1,1,-1,-1,1,-1,1,-1
 *   fact signum-long 0,1,-1,1,-1,-1,1,-1,1,-1
 *   fact cmpu-mismatches 0
 *   fact signum-mismatches 0
 *   fact warm true
 *
 * (Every line is integer arithmetic fixed by the javadoc, so it is HotSpot's
 * on any JVM; the values were computed off-host, confirm on the reference run.)
 *
 * Engagement (switch =1): CRATONVM_DBG_IR_COMPILES=1 prints
 * "[ir] pure-expansions J27W8IntrinsicRows.cmpuI(II)I: [CompareUnsignedI]" and
 * the cmpuL / signumI / signumL twins.
 * Timing (stderr, "row <name> <ns/op>") only with -Dj27.time=true.
 */
public final class J27W8IntrinsicRows {
    static final int WARM = 60_000;

    static final int[] INTS = {
        0, 1, -1, 2, -2, Integer.MIN_VALUE, Integer.MAX_VALUE, Integer.MIN_VALUE + 1,
        0x7FFF_0000, 0x8000_FFFF,
    };
    static final long[] LONGS = {
        0L, 1L, -1L, 2L, -2L, Long.MIN_VALUE, Long.MAX_VALUE, Long.MIN_VALUE + 1,
        0x1_0000_0000L, 0x8000_0000_FFFF_FFFFL,
    };

    static long seed = 0x2545F4914F6CDD1DL;

    static long next() {
        seed ^= seed << 13;
        seed ^= seed >>> 7;
        seed ^= seed << 17;
        return seed;
    }

    // ---- the expanded sites ----
    static int cmpuI(int a, int b) {
        return Integer.compareUnsigned(a, b);
    }

    static int cmpuL(long a, long b) {
        return Long.compareUnsigned(a, b);
    }

    static int signumI(int v) {
        return Integer.signum(v);
    }

    static int signumL(long v) {
        return Long.signum(v);
    }

    // ---- independent references (no call to the methods under test) ----
    static int refCmpuI(int a, int b) {
        long x = a & 0xFFFF_FFFFL;
        long y = b & 0xFFFF_FFFFL;
        return x < y ? -1 : (x == y ? 0 : 1);
    }

    static int refCmpuL(long a, long b) {
        if (a == b) {
            return 0;
        }
        long ha = a >>> 1;
        long hb = b >>> 1;
        if (ha != hb) {
            return ha < hb ? -1 : 1;
        }
        return (a & 1) < (b & 1) ? -1 : 1;
    }

    static int refSignum(long v) {
        return v > 0 ? 1 : (v < 0 ? -1 : 0);
    }

    static int bad;

    static void checkPair(int a, int b, long c, long d) {
        if (cmpuI(a, b) != refCmpuI(a, b)) bad++;
        if (cmpuL(c, d) != refCmpuL(c, d)) bad++;
    }

    static int signBad;

    static void checkSign(int a, long c) {
        if (signumI(a) != refSignum(a)) signBad++;
        if (signumL(c) != refSignum(c)) signBad++;
    }

    static void warm() {
        for (int k = 0; k < WARM; k++) {
            long r = next();
            long s = next();
            int i = k % INTS.length;
            int j = (k / INTS.length) % INTS.length;
            checkPair((int) r, (int) s, r, s);
            checkPair(INTS[i], INTS[j], LONGS[i], LONGS[j]);
            checkPair((int) r, INTS[j], r, LONGS[j]);
            checkSign((int) r, s);
            checkSign(INTS[i], LONGS[i]);
        }
    }

    static String signRow(boolean wide) {
        StringBuilder sb = new StringBuilder();
        for (int k = 0; k < INTS.length; k++) {
            if (k > 0) sb.append(',');
            sb.append(wide ? signumL(LONGS[k]) : signumI(INTS[k]));
        }
        return sb.toString();
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    public static void main(String[] args) {
        warm();
        int min = Integer.MIN_VALUE;
        int max = Integer.MAX_VALUE;
        fact("cmpu-int-neg1-0", cmpuI(-1, 0));
        fact("cmpu-int-0-neg1", cmpuI(0, -1));
        fact("cmpu-int-min-max", cmpuI(min, max));
        fact("cmpu-int-max-min", cmpuI(max, min));
        fact("cmpu-int-min-min", cmpuI(min, min));
        fact("cmpu-int-1-2", cmpuI(1, 2));
        fact("cmpu-int-neg2-neg1", cmpuI(-2, -1));
        fact("cmpu-long-neg1-0", cmpuL(-1L, 0L));
        fact("cmpu-long-min-max", cmpuL(Long.MIN_VALUE, Long.MAX_VALUE));
        fact("cmpu-long-max-min", cmpuL(Long.MAX_VALUE, Long.MIN_VALUE));
        fact("cmpu-long-eq", cmpuL(-5L, -5L));
        fact("cmpu-long-hi32", cmpuL(0x1_0000_0000L, 0xFFFF_FFFFL));
        fact("cmpu-long-low-vs-neg", cmpuL(0x7FFF_FFFF_FFFF_FFFFL, 0x8000_0000_0000_0000L));
        fact("signum-int", signRow(false));
        fact("signum-long", signRow(true));
        fact("cmpu-mismatches", bad);
        fact("signum-mismatches", signBad);
        fact("warm", true);
        if (Boolean.getBoolean("j27.time")) {
            time();
        }
    }

    static void time() {
        int n = 20_000_000;
        long t0 = System.nanoTime();
        int acc = 0;
        for (int k = 0; k < n; k++) {
            acc += cmpuI(k * 0x9E3779B9, k);
        }
        long t1 = System.nanoTime();
        long accL = 0;
        for (int k = 0; k < n; k++) {
            accL += signumL((long) k * 0x9E3779B97F4A7C15L);
        }
        long t2 = System.nanoTime();
        System.err.println("row cmpu-int " + (double) (t1 - t0) / n + " (" + acc + ")");
        System.err.println("row signum-long " + (double) (t2 - t1) / n + " (" + accL + ")");
    }
}
