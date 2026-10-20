// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 66, lane L7: ns per loop iteration for the
// shapes CRATONVM_INTERP_PUTSTATIC_INLINE_HIT targets (the `putstatic` arm
// stores a quickened static itself instead of calling `op_putstatic`;
// proposal
// i65-L7-proposal-the-putstatic-arm-stores-a-quickened-site-inline-20261029).
// Measure with CRATONVM_INTERP_GETSTATIC_INLINE_HIT=1 held on both sides
// (the `getstatic` half of each read-modify-write is then already inline),
// switch off / on.
//
// Rows (1 000 000 iterations each):
//   counterBody  `counter++` on a static int. Expected down.
//   seqBody      `seq += 1` on a static long (the KIND_LONG pop). Down.
//   totalBody    `total += 0.5` on a static double (the KIND_DOUBLE pop).
//                Down.
//   volatileBody `vcount++` on a volatile static int (two SeqCst fences per
//                store either way). Down by less.
//   refBody      `last = arrs[i & 3]` (a reference store: SATB pre-barrier
//                in `set_static_shared` either way). Down.
//   localBound   `s += i & 3`, `i < n` (control: no static; flat).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare switch off / on on the
// SAME fat-LTO binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W66PutstaticInlineBench
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// counterBody checksum=1000000
// seqBody checksum=1000000
// totalBody checksum=500000
// volatileBody checksum=1000000
// refBody checksum=4
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
public class L7W66PutstaticInlineBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static int counter;
    static long seq;
    static double total;
    static volatile int vcount;
    static int[] last;
    static final int[][] ARRS = {new int[1], new int[2], new int[3], new int[4]};

    static int counterBody(int n) {
        counter = 0;
        for (int i = 0; i < n; i++) {
            counter++;
        }
        return counter;
    }

    static int seqBody(int n) {
        seq = 0;
        for (int i = 0; i < n; i++) {
            seq += 1;
        }
        return (int) (seq ^ (seq >>> 32));
    }

    static int totalBody(int n) {
        total = 0;
        for (int i = 0; i < n; i++) {
            total += 0.5;
        }
        return (int) total;
    }

    static int volatileBody(int n) {
        vcount = 0;
        for (int i = 0; i < n; i++) {
            vcount++;
        }
        return vcount;
    }

    static int refBody(int n) {
        int[][] arrs = ARRS;
        for (int i = 0; i < n; i++) {
            last = arrs[i & 3];
        }
        return last.length;
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
        row("counterBody", () -> counterBody(n));
        row("seqBody", () -> seqBody(n));
        row("totalBody", () -> totalBody(n));
        row("volatileBody", () -> volatileBody(n));
        row("refBody", () -> refBody(n));
        row("localBound", () -> localBound(n));
    }
}
