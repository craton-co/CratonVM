// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 66, lane L7: ns per loop iteration for the
// shapes CRATONVM_INTERP_FUSED_GETSTATIC_IFZERO targets (`getstatic #F;
// if<cond>` over a quickened int static fused into one dispatch; proposal
// i65-L7-proposal-a-fused-getstatic-and-if-zero-branch-for-static-flags-20261029).
// Measure with CRATONVM_INTERP_GETSTATIC_INLINE_HIT=1 held on both sides,
// switch off / on.
//
// Rows (1 000 000 iterations each):
//   flagBody     `if (flag) s += i & 3` (static boolean, ifeq). Expected down.
//   runningBody  `while (running && i < n) i++` (top test: ifeq). Down.
//   levelBody    `if (level > 0) s++` (static int, ifle). Down.
//   doWhileBody  `do { ... } while (dflag && ...)` (a backward fused ifeq,
//                through the back-edge accounting). Down.
//   localBound   `s += i & 3`, `i < n` (control: no static; flat).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare switch off / on on the
// SAME fat-LTO binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W66GetstaticIfZeroBench
// stdout is deterministic, identical on HotSpot 25 (and under every switch):
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// flagBody checksum=1500000
// runningBody checksum=1000000
// levelBody checksum=1000000
// doWhileBody checksum=1000000
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
public class L7W66GetstaticIfZeroBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static boolean flag = true;
    static boolean running = true;
    static boolean dflag = true;
    static int level = 1;

    static int flagBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            if (flag) {
                s += i & 3;
            }
        }
        return s;
    }

    static int runningBody(int n) {
        int i = 0;
        while (running && i < n) {
            i++;
        }
        return i;
    }

    static int levelBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            if (level > 0) {
                s++;
            }
        }
        return s;
    }

    static int doWhileBody(int n) {
        int i = 0;
        do {
            i++;
        } while (dflag && i < n);
        return i;
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
        row("flagBody", () -> flagBody(n));
        row("runningBody", () -> runningBody(n));
        row("levelBody", () -> levelBody(n));
        row("doWhileBody", () -> doWhileBody(n));
        row("localBound", () -> localBound(n));
    }
}
