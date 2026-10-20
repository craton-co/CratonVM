// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L7: ns per loop iteration for
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_STATIC_OPERAND  `a[j] += SF`
//       (proposal i75-L7-proposal-fuse-the-static-operand-of-the-array-
//       increment-20261108; default off)
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND   `a[j] += b[j]`,
//       `a[j] = a[j] + b[j]` (proposal i75-L7-proposal-fuse-the-array-
//       element-operand-of-the-array-increment-20261108; default off)
// and, cross-binary against wave 75, the wave-76 loop-frame refactor (the
// decline-memo test moved into the entries; one wide-local entry): the hit
// rows must stay flat.
//
// Every row runs `j = i & 7` and one group per iteration over an 8-element
// array. Rows (1 000 000 iterations each):
//   staticDecl   `a[j] += SF` (int static). STATIC_OPERAND: a hit.
//   fieldStatic  `counts[j] += SF` (the stack-top entry). STATIC_OPERAND: hit.
//   volStatic    `a[j] += VF` (a volatile int static). STATIC_OPERAND: hit.
//   longStatic   `la[j] += LS` (a long static). STATIC_OPERAND: hit.
//   addArr       `a[j] += b[j]`. ARRAY_OPERAND: a hit.
//   twoLoadsMix  `a[j] = a[j] + b[j]` (no dup2). ARRAY_OPERAND: a hit.
//   byteArr      `a[j] += bb[j]` (a byte[] operand). ARRAY_OPERAND: a hit.
//   longArr      `la[j] += lb[j]`. ARRAY_OPERAND: a hit.
//   intInc       `a[j]++` (a hit already). Flat.
//   twoLoads     `a[j] = a[j] + 1` (a no-dup hit). Flat.
//   fieldArray   `counts[j]++` (a stack-top hit). Flat.
//   arrayRead    `s += a[j]` (no increment). Flat.
//   localInc     `s += j` (no array). Flat.
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the env sets on the
// SAME binary, interleaved, several rounds, and take the median of the
// per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W76ArrayIncrementOperandBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of 9)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// staticDecl checksum=13500000
// fieldStatic checksum=13500000
// volStatic checksum=22500000
// longStatic checksum=13500000
// addArr checksum=4500000
// twoLoadsMix checksum=4500000
// byteArr checksum=9000000
// longArr checksum=4500000
// intInc checksum=4500000
// twoLoads checksum=4500000
// fieldArray checksum=4500000
// arrayRead checksum=1000000
// localInc checksum=3500000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W76ArrayIncrementOperandBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static final int[] IA = new int[8];
    static final int[] IB = new int[8];
    static final byte[] BB = new byte[8];
    static final long[] LA = new long[8];
    static final long[] LB = new long[8];

    static int SF = 3;
    static volatile int VF = 5;
    static long LS = 3L;

    int[] counts = new int[8];

    /** Sum of `a[k] * (k + 1)`. */
    static int weigh(int[] a) {
        int s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return s;
    }

    /** Sum of `la[k] * (k + 1)`, folded to an int. */
    static int weighLong(long[] la) {
        long s = 0;
        for (int k = 0; k < la.length; k++) {
            s += la[k] * (k + 1);
        }
        return (int) (s ^ (s >>> 32));
    }

    // Locals in every static row: the array 0, n 1, j 2, i 3 (`aload_0;
    // iload_2` opens each group, so the local entry is what runs).
    static int staticDecl(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] += SF;
        }
        return weigh(a);
    }

    // Locals: this 0, n 1, j 2, i 3.
    int fieldStatic(int n) {
        java.util.Arrays.fill(counts, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            counts[j] += SF;
        }
        return weigh(counts);
    }

    static int volStatic(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] += VF;
        }
        return weigh(a);
    }

    static int longStatic(long[] la, int n) {
        java.util.Arrays.fill(la, 0L);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            la[j] += LS;
        }
        return weighLong(la);
    }

    // Locals: a 0, b 1, j 2, i 3.
    static int addArr(int[] a, int[] b) {
        java.util.Arrays.fill(a, 0);
        java.util.Arrays.fill(b, 1);
        int j = 0;
        for (int i = 0; i < ITERS; i++) {
            j = i & 7;
            a[j] += b[j];
        }
        return weigh(a);
    }

    // Locals: a 0, b 1, j 2, i 3.
    static int twoLoadsMix(int[] a, int[] b) {
        java.util.Arrays.fill(a, 0);
        java.util.Arrays.fill(b, 1);
        int j = 0;
        for (int i = 0; i < ITERS; i++) {
            j = i & 7;
            a[j] = a[j] + b[j];
        }
        return weigh(a);
    }

    // Locals: a 0, b 1, j 2, i 3.
    static int byteArr(int[] a, byte[] b) {
        java.util.Arrays.fill(a, 0);
        java.util.Arrays.fill(b, (byte) 2);
        int j = 0;
        for (int i = 0; i < ITERS; i++) {
            j = i & 7;
            a[j] += b[j];
        }
        return weigh(a);
    }

    // Locals: la 0, lb 1, j 2, i 3.
    static int longArr(long[] la, long[] lb) {
        java.util.Arrays.fill(la, 0L);
        java.util.Arrays.fill(lb, 1L);
        int j = 0;
        for (int i = 0; i < ITERS; i++) {
            j = i & 7;
            la[j] += lb[j];
        }
        return weighLong(la);
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

    static int twoLoads(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] = a[j] + 1;
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
        final L7W76ArrayIncrementOperandBench t = new L7W76ArrayIncrementOperandBench();
        row("staticDecl", () -> staticDecl(IA, n));
        row("fieldStatic", () -> t.fieldStatic(n));
        row("volStatic", () -> volStatic(IA, n));
        row("longStatic", () -> longStatic(LA, n));
        row("addArr", () -> addArr(IA, IB));
        row("twoLoadsMix", () -> twoLoadsMix(IA, IB));
        row("byteArr", () -> byteArr(IA, BB));
        row("longArr", () -> longArr(LA, LB));
        row("intInc", () -> intInc(IA, n));
        row("twoLoads", () -> twoLoads(IA, n));
        row("fieldArray", () -> t.fieldArray(n));
        row("arrayRead", () -> arrayRead(IA, n));
        row("localInc", () -> localInc(n));
    }
}
