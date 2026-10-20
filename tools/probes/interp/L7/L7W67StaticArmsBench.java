// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 67, lane L7: ns per loop iteration for the four
// opcodes that shared one fast-path arm (`getstatic`, `putstatic`, `new`,
// `anewarray`) and now have arms of their own (proposal
// i66-L7-proposal-getstatic-and-putstatic-get-arms-of-their-own-20261030).
// No switch: compare the binary before / after the split (commit
// "getstatic, putstatic and new/anewarray get arms of their own").
//
// Rows (1 000 000 iterations each):
//   getBody      `s += LIMIT_A` (a quickened int getstatic). Expected down.
//   putBody      `sink = i` (a quickened int putstatic). Expected down.
//   newBody      `o = new Object()` (`new` no longer pays the static
//                opcodes' compares). Expected down or flat.
//   anewBody     `a = new Object[1]` (`anewarray`). Down or flat.
//   localBound   `s += i & 3` (control: none of the four; flat).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the two binaries
// interleaved, several rounds, and take the median of the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W67StaticArmsBench
// stdout is deterministic, identical on HotSpot 25:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// getBody checksum=3000000
// putBody checksum=999999
// newBody checksum=1000000
// anewBody checksum=1
// localBound checksum=1500000
// HOTSPOT_EXPECTED_END
// stderr: `<row> <x> ns/op (min of 9)`, one line per row.
public class L7W67StaticArmsBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static int limitA = 3;
    static int sink;

    static int getBody(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += limitA;
        }
        return s;
    }

    static int putBody(int n) {
        for (int i = 0; i < n; i++) {
            sink = i;
        }
        return sink;
    }

    static int newBody(int n) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            Object o = new Object();
            if (o != null) {
                c++;
            }
        }
        return c;
    }

    static int anewBody(int n) {
        Object[] a = null;
        for (int i = 0; i < n; i++) {
            a = new Object[1];
        }
        return a.length;
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
        row("getBody", () -> getBody(n));
        row("putBody", () -> putBody(n));
        row("newBody", () -> newBody(n));
        row("anewBody", () -> anewBody(n));
        row("localBound", () -> localBound(n));
    }
}
