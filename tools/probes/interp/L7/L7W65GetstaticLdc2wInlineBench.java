// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 65, lane L7: ns per loop iteration for the
// shapes the two wave-65 switches target, plus the wave-64 `ldc` rows for a
// clean rerun of CRATONVM_INTERP_LDC_INLINE_HIT (its `ldcBoth` +26% was
// unexplained).
//
//   CRATONVM_INTERP_GETSTATIC_INLINE_HIT=1  the `getstatic` arm pushes a
//       quickened static itself instead of calling `op_getstatic` (proposal
//       i64-L7-proposal-the-getstatic-arm-answers-a-quickened-site-inline-20261028).
//   CRATONVM_INTERP_LDC2W_INLINE_HIT=1      the `ldc2_w` arm pushes a warm
//       long / double itself instead of calling `execute_ldc2w` (proposal
//       i64-L7-proposal-the-ldc2w-arm-answers-a-warm-long-or-double-inline-20261028).
//
// Rows (1 000 000 iterations each):
//   staticBody   `s += step` (a non-final static int in the body).
//                GETSTATIC_INLINE_HIT: expected down.
//   flagBody     `if (flag) s += i & 3` (a static boolean). GETSTATIC: down.
//   longStatic   `h += lstep` (a static long: the KIND_LONG push). GETSTATIC:
//                down.
//   refStatic    `s += table[i & 3]` (a static int[]: the reference push).
//                GETSTATIC: down.
//   hashBody     `h = h * 0x9E3779B97F4A7C15L + i`. LDC2W_INLINE_HIT: down.
//   doubleBody   `d += i * 0.5`. LDC2W: down.
//   longBound    `for (long i = 0; i < 1_000_000L; i++)` (an `ldc2_w`
//                bound: no fused long compare exists). LDC2W: down.
//   ldcBody      `s += 100_003` (wave 64's row). LDC_INLINE_HIT: down.
//   ldcBoth      `i < 1_000_000` and `s += 100_003 + (i & 3)` (wave 64's
//                row). LDC_INLINE_HIT: expected down; was +26% on the
//                wave-64 host run.
//   localBound   `i < n`, `n` a parameter (control: flat under every
//                switch).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare switch off / on on the
// SAME fat-LTO binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W65GetstaticLdc2wInlineBench
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// staticBody checksum=3000000
// flagBody checksum=1500000
// longStatic checksum=3000000
// refStatic checksum=2500000
// hashBody checksum=-191114706
// doubleBody checksum=249999750
// longBound checksum=1500000
// ldcBody checksum=1218752192
// ldcBoth checksum=1220252192
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
public class L7W65GetstaticLdc2wInlineBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static int step = 3;
    static boolean flag = true;
    static long lstep = 3;
    static int[] table = {1, 2, 3, 4};

    static int staticBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += step;
        }
        return s;
    }

    static int flagBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            if (flag) {
                s += i & 3;
            }
        }
        return s;
    }

    static int longStatic(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            h += lstep;
        }
        return (int) (h ^ (h >>> 32));
    }

    static int refStatic(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += table[i & 3];
        }
        return s;
    }

    static int hashBody(int n) {
        long h = 0;
        for (int i = 0; i < n; i++) {
            h = h * 0x9E3779B97F4A7C15L + i;
        }
        return (int) (h ^ (h >>> 32));
    }

    static int doubleBody(int n) {
        double d = 0;
        for (int i = 0; i < n; i++) {
            d += i * 0.5;
        }
        return (int) (d / 1000);
    }

    static int longBound() {
        int s = 0;
        for (long i = 0; i < 1_000_000L; i++) {
            s += (int) i & 3;
        }
        return s;
    }

    static int ldcBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += 100_003;
        }
        return s;
    }

    static int ldcBoth() {
        int s = 0;
        for (int i = 0; i < 1_000_000; i++) {
            s += 100_003 + (i & 3);
        }
        return s;
    }

    static int localBound(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i & 3;
        }
        return s;
    }

    static void row(String name, Row r) {
        int checksum = r.run(); // untimed warm-up: fills the sites
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            int c = r.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.err.printf("%-13s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        final int n = ITERS;
        row("staticBody", () -> staticBody(n));
        row("flagBody", () -> flagBody(n));
        row("longStatic", () -> longStatic(n));
        row("refStatic", () -> refStatic(n));
        row("hashBody", () -> hashBody(n));
        row("doubleBody", () -> doubleBody(n));
        row("longBound", L7W65GetstaticLdc2wInlineBench::longBound);
        row("ldcBody", () -> ldcBody(n));
        row("ldcBoth", L7W65GetstaticLdc2wInlineBench::ldcBoth);
        row("localBound", () -> localBound(n));
    }
}
