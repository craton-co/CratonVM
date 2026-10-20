// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 68, lane L7: ns per loop iteration for the
// shapes CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE adds to the fused
// static increment (narrow `B` / `C` / `S` statics behind their own i2b /
// i2c / i2s, and `long` statics with an lconst / lload operand; proposal
// i67-L7-proposal-fused-static-increment-for-narrow-and-long-statics-20261031).
// Measure with the now-default CRATONVM_INTERP_FUSED_STATIC_INCREMENT held on
// both sides, the new switch off / on.
//
// Rows (1 000 000 iterations each):
//   byteBody     `bb++` on a static byte (iconst_1; iadd; i2b). Down.
//   shortBody    `ss += 1000` (sipush; iadd; i2s). Down.
//   charBody     `cc++` (iconst_1; iadd; i2c). Down.
//   seqBody      `seq++` on a static long (lconst_1; ladd). Down.
//   longLocal    `tot += k`, `k` a long local (lload_N; ladd). Down.
//   longLdc      `big += 1_000_000_007L` (ldc2_w: never fused; control,
//                flat).
//   intBody      `counter++` on a static int (the wave-67 shape; control,
//                flat).
//   pairBody     `x = y + 1` (two pool indices: never fused; the wave-67
//                regression row, flat under this switch).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare switch off / on on the
// SAME fat-LTO binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W68FusedStaticIncrementWideBench
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// byteBody checksum=64
// shortBody checksum=-13824
// charBody checksum=16960
// seqBody checksum=1000000
// longLocal checksum=3000000
// longLdc checksum=1000000
// intBody checksum=1000000
// pairBody checksum=1000000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
public class L7W68FusedStaticIncrementWideBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static byte bb;
    static short ss;
    static char cc;
    static long seq;
    static long tot;
    static long big;
    static int counter;
    static int x;
    static int y;

    static int byteBody(int n) {
        bb = 0;
        for (int i = 0; i < n; i++) {
            bb++;
        }
        return bb;
    }

    static int shortBody(int n) {
        ss = 0;
        for (int i = 0; i < n; i++) {
            ss += 1000;
        }
        return ss;
    }

    static int charBody(int n) {
        cc = 0;
        for (int i = 0; i < n; i++) {
            cc++;
        }
        return cc;
    }

    static int seqBody(int n) {
        seq = 0;
        for (int i = 0; i < n; i++) {
            seq++;
        }
        return (int) seq;
    }

    static int longLocal(int n) {
        long k = 3;
        tot = 0;
        for (int i = 0; i < n; i++) {
            tot += k;
        }
        return (int) tot;
    }

    static int longLdc(int n) {
        big = 0;
        for (int i = 0; i < n; i++) {
            big += 1_000_000_007L;
        }
        return (int) (big / 1_000_000_007L);
    }

    static int intBody(int n) {
        counter = 0;
        for (int i = 0; i < n; i++) {
            counter++;
        }
        return counter;
    }

    static int pairBody(int n) {
        y = 0;
        for (int i = 0; i < n; i++) {
            x = y + 1;
            y = x;
        }
        return y;
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
        row("byteBody", () -> byteBody(n));
        row("shortBody", () -> shortBody(n));
        row("charBody", () -> charBody(n));
        row("seqBody", () -> seqBody(n));
        row("longLocal", () -> longLocal(n));
        row("longLdc", () -> longLdc(n));
        row("intBody", () -> intBody(n));
        row("pairBody", () -> pairBody(n));
    }
}
