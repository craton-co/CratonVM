// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L7: ns per loop iteration for
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_SHIFT_DIV  the fused array
//       increment also takes the shifts and divisions, so the compound
//       shapes the default-on increment declined (`a[j] <<= 1`, `a[j] /= 3`,
//       wave 73: ~+16% against the increment switched off) become hits
//       (default off);
//   CRATONVM_INTERP_FUSED_ARRAY_UPDATE_NO_DUP  `a[j] = a[j] + 1` (no `dup2`)
//       in one dispatch (default off; proposal
//       i73-L7-proposal-fuse-the-array-element-update-without-dup2-20261106).
// The wave-72 array increment switches (base, VALUE, TOP) are on by
// default; DUP2 and SCREEN are off.
//
// Every row runs `j = i & 7` (or `i & 7` inline) and one group per
// iteration over an 8-element array unless said otherwise.
// Rows (1 000 000 iterations each):
//   shiftDecl     `a[j] <<= 1`. SHIFT_DIV: a hit (the wave-73 row).
//   shrDecl       `a[j] >>= s` (count from a local). SHIFT_DIV: a hit.
//   divDecl       `a[j] /= 3` (the wave-73 row). SHIFT_DIV: a hit.
//   remDecl       `a[j] += i; a[j] %= 1009` (a hit, then an `irem`).
//                 SHIFT_DIV: both hits.
//   longShift     `la[j] <<= 1` on a long[] (int count). SHIFT_DIV: a hit.
//   fieldShift    `counts[j] <<= 1` on a field array (the TOP entry).
//                 SHIFT_DIV: a hit.
//   histShift     `h[i & 7] <<= 1` (the DUP2 entry, off by default).
//   staticDecl    `a[j] += SF` (a getstatic operand: declined under every
//                 switch; the control for what is still declined).
//   intInc        `a[j]++` (a hit). Flat under both switches.
//   fieldArray    `counts[j]++` (a TOP hit). Flat.
//   twoLoads      `a[j] = a[j] + 1`. NO_DUP: a hit.
//   twoLoadsShift `a[j] = a[j] << 1`. NO_DUP + SHIFT_DIV: a hit.
//   twoLoadsMix   `a[j] = a[j] + b[j]` (the no-dup prefix whose operand is
//                 not fused: NO_DUP's decline cost). Flat, or +1 call.
//   arrayRead     `s += a[j]` (the accumulate hit). Flat.
//   localInc      `s += j` (no array). Flat.
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the env sets on the
// SAME binary, interleaved, several rounds, and take the median of the
// per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W74ArrayShiftDivNoDupBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of 9)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// shiftDecl checksum=0
// shrDecl checksum=-36
// divDecl checksum=0
// remDecl checksum=17780
// longShift checksum=0
// fieldShift checksum=0
// histShift checksum=0
// staticDecl checksum=13500000
// intInc checksum=4500000
// fieldArray checksum=4500000
// twoLoads checksum=4500000
// twoLoadsShift checksum=0
// twoLoadsMix checksum=4500000
// arrayRead checksum=1000000
// localInc checksum=3500000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W74ArrayShiftDivNoDupBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static final int[] IA = new int[8];
    static final int[] IB = new int[8];
    static final long[] LA = new long[8];

    static int SF = 3;

    int[] counts = new int[8];

    /** Sum of `a[k] * (k + 1)`. */
    static int weigh(int[] a) {
        int s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return s;
    }

    // Locals in every static row: the array 0, n 1, j 2, i 3 (`aload_0;
    // iload_2` opens each group, so the local entry is what runs).
    static int shiftDecl(int[] a, int n) {
        java.util.Arrays.fill(a, 1);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] <<= 1;
        }
        return weigh(a);
    }

    // Locals: a 0, n 1, j 2, s 3, i 4 (`iload_3` is the count).
    static int shrDecl(int[] a, int n) {
        java.util.Arrays.fill(a, -1024);
        int j = 0;
        int s = 1;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] >>= s;
        }
        return weigh(a);
    }

    static int divDecl(int[] a, int n) {
        java.util.Arrays.fill(a, 1 << 30);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] /= 3;
        }
        return weigh(a);
    }

    static int remDecl(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] += i;
            a[j] %= 1009;
        }
        return weigh(a);
    }

    static int longShift(long[] la, int n) {
        java.util.Arrays.fill(la, 1L);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            la[j] <<= 1;
        }
        long s = 0;
        for (int k = 0; k < la.length; k++) {
            s += la[k] * (k + 1);
        }
        return (int) (s ^ (s >>> 32));
    }

    int fieldShift(int n) {
        java.util.Arrays.fill(counts, 1);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            counts[j] <<= 1;
        }
        return weigh(counts);
    }

    static int histShift(int[] h, int n) {
        java.util.Arrays.fill(h, 1);
        for (int i = 0; i < n; i++) {
            h[i & 7] <<= 1;
        }
        return weigh(h);
    }

    static int staticDecl(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] += SF;
        }
        return weigh(a);
    }

    static int intInc(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j]++;
        }
        return weigh(a);
    }

    int fieldArray(int n) {
        java.util.Arrays.fill(counts, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            counts[j]++;
        }
        return weigh(counts);
    }

    static int twoLoads(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] = a[j] + 1;
        }
        return weigh(a);
    }

    static int twoLoadsShift(int[] a, int n) {
        java.util.Arrays.fill(a, 1);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] = a[j] << 1;
        }
        return weigh(a);
    }

    // Locals: a 0, b 1, n 2, j 3, i 4.
    static int twoLoadsMix(int[] a, int[] b, int n) {
        java.util.Arrays.fill(a, 0);
        java.util.Arrays.fill(b, 1);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] = a[j] + b[j];
        }
        return weigh(a);
    }

    static int arrayRead(int[] a, int n) {
        java.util.Arrays.fill(a, 1);
        int j = 0;
        int s = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            s += a[j];
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
        final int n = ITERS;
        final L7W74ArrayShiftDivNoDupBench t = new L7W74ArrayShiftDivNoDupBench();
        row("shiftDecl", () -> shiftDecl(IA, n));
        row("shrDecl", () -> shrDecl(IA, n));
        row("divDecl", () -> divDecl(IA, n));
        row("remDecl", () -> remDecl(IA, n));
        row("longShift", () -> longShift(LA, n));
        row("fieldShift", () -> t.fieldShift(n));
        row("histShift", () -> histShift(IA, n));
        row("staticDecl", () -> staticDecl(IA, n));
        row("intInc", () -> intInc(IA, n));
        row("fieldArray", () -> t.fieldArray(n));
        row("twoLoads", () -> twoLoads(IA, n));
        row("twoLoadsShift", () -> twoLoadsShift(IA, n));
        row("twoLoadsMix", () -> twoLoadsMix(IA, IB, n));
        row("arrayRead", () -> arrayRead(IA, n));
        row("localInc", () -> localInc(n));
    }
}
