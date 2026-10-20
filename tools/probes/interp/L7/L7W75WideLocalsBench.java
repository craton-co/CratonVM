// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L7: ns per element for
//   CRATONVM_INTERP_FUSED_WIDE_LOCALS  the element read `a[i]` fuses when a
//       local is past 3: `aload_N; iload n; <x>aload` (the for-each loop's
//       hidden index) and `aload n; iload{_M, n}; <x>aload` (default off;
//       proposal interpreter-i72-L7-proposal-wide-local-forms-of-the-fused-groups-KEPT-OFF-20261019).
//
// Rows (1 000 000 elements each):
//   forEachSum   `for (int v : a) s += v` in an instance method with one
//                parameter: `aload_3; iload 5; iaload`. WIDE: one dispatch
//                where it was two.
//   forEachHash  `h = 31 * h + (v & 0xff)` over a `byte[]` (the shape of
//                `Arrays.hashCode(byte[])` and the JDK's Latin-1 hash loop).
//   wideBoth     `s += b[i]` with the array and the index past 3: `aload 4;
//                iload 6; iaload`.
//   wideArr      `s += a[j]` with the array past 3 and the index in 0..3:
//                `aload 4; iload_0; iaload`.
//   shortSum     `s += a[i]` with every local in 0..3 (the wave-73
//                accumulate hit). Flat (control).
//   localInc     `s += j` (no array). Flat (control).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the env sets on the
// SAME binary, interleaved, several rounds, and take the median of the
// per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W75WideLocalsBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of 9)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// forEachSum checksum=1000000
// forEachHash checksum=321714401
// wideBoth checksum=1000000
// wideArr checksum=1000000
// shortSum checksum=1000000
// localInc checksum=3500000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W75WideLocalsBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static final int[] ONES = new int[ITERS];
    static final byte[] BYTES = new byte[ITERS];
    static final int[] IA = new int[8];

    // Locals: this 0, a 1, s 2, then the for-each's array 3, length 4,
    // index 5, element 6.
    int forEachSum(int[] a) {
        int s = 0;
        for (int v : a) {
            s += v;
        }
        return s;
    }

    // Locals: this 0, b 1, h 2, array 3, length 4, index 5, element 6.
    int forEachHash(byte[] b) {
        int h = 1;
        for (byte v : b) {
            h = 31 * h + (v & 0xff);
        }
        return h;
    }

    // Locals: a 0, n 1, x 2, y 3, b 4, s 5, i 6.
    static int wideBoth(int[] a, int n, int x, int y) {
        int[] b = a;
        int s = x + y;
        for (int i = 0; i < n; i++) {
            s += b[i];
        }
        return s;
    }

    // Locals: j 0, n 1, s 2, i 3, a 4.
    static int wideArr(int j, int n, int s, int i, int[] a) {
        for (i = 0; i < n; i++) {
            j = i & 7;
            s += a[j];
        }
        return s;
    }

    // Locals: a 0, n 1, s 2, i 3.
    static int shortSum(int[] a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += a[i];
        }
        return s;
    }

    static int localInc(int n) {
        int j = 0;
        int s = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            s += j;
        }
        return s;
    }

    static void row(String name, Row r) {
        int checksum = r.run(); // untimed warm-up
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
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        java.util.Arrays.fill(ONES, 1);
        for (int k = 0; k < ITERS; k++) {
            BYTES[k] = (byte) (k * 7);
        }
        java.util.Arrays.fill(IA, 1);
        final int n = ITERS;
        final L7W75WideLocalsBench t = new L7W75WideLocalsBench();
        row("forEachSum", () -> t.forEachSum(ONES));
        row("forEachHash", () -> t.forEachHash(BYTES));
        row("wideBoth", () -> wideBoth(ONES, n, 0, 0));
        row("wideArr", () -> wideArr(0, n, 0, 0, IA));
        row("shortSum", () -> shortSum(ONES, n));
        row("localInc", () -> localInc(n));
    }
}
