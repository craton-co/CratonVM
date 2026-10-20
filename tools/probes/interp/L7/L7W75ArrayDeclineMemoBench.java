// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L7: ns per loop iteration for
//   CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO  the fused array increment
//       remembers, per method and pc, a group its decoder refused on its
//       bytes, so the dispatch loop stops calling the helper there (default
//       off; proposal
//       i74-L7-proposal-a-per-method-array-increment-decline-memo-20261107).
// The array increment switches on by default (base, VALUE, TOP, SHIFT_DIV,
// NO_DUP) stay as they are; DUP2 is off.
//
// Every row runs `j = i & 7` and one group per iteration over an 8-element
// array. Rows (1 000 000 iterations each):
//   staticDecl  `a[j] += SF` (a getstatic operand): the local entry AND the
//               stack-top entry decline, two calls per iteration. MEMO: none.
//   fieldStatic `counts[j] += SF` (the stack-top entry declines). MEMO: none.
//   addArr      `a[j] += b[j]` (an array operand). MEMO: none.
//   twoLoadsMix `a[j] = a[j] + b[j]` (the no-dup prefix whose operand is not
//               fused). MEMO: none.
//   longWiden   `la[j] += i` (`iload; i2l`). MEMO: none.
//   floatInc    `f[j] += 1f` (no float arithmetic is fused). MEMO: none.
//   mixed       `a[j] += SF; a[j]++` (a declining and a fused site in one
//               method: the memo is per pc). MEMO: the first group's call.
//   intInc      `a[j]++` (a hit). Flat (the bit test on the hit path).
//   twoLoads    `a[j] = a[j] + 1` (a no-dup hit). Flat.
//   fieldArray  `counts[j]++` (a stack-top hit). Flat.
//   arrayRead   `s += a[j]` (no increment). Flat.
//   localInc    `s += j` (no array). Flat.
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the env sets on the
// SAME binary, interleaved, several rounds, and take the median of the
// per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W75ArrayDeclineMemoBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of 9)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// staticDecl checksum=13500000
// fieldStatic checksum=13500000
// addArr checksum=4500000
// twoLoadsMix checksum=4500000
// longWiden checksum=-559863605
// floatInc checksum=4500000
// mixed checksum=18000000
// intInc checksum=4500000
// twoLoads checksum=4500000
// fieldArray checksum=4500000
// arrayRead checksum=1000000
// localInc checksum=3500000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W75ArrayDeclineMemoBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static final int[] IA = new int[8];
    static final int[] IB = new int[8];
    static final long[] LA = new long[8];
    static final float[] FA = new float[8];

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
    static int staticDecl(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] += SF;
        }
        return weigh(a);
    }

    int fieldStatic(int n) {
        java.util.Arrays.fill(counts, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            counts[j] += SF;
        }
        return weigh(counts);
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

    // Locals: la 0, n 1, j 2, i 3. The weighted sum (2 250 003 000 000)
    // is folded to an int as `longShift` in the W74 bench folds it.
    static int longWiden(long[] la, int n) {
        java.util.Arrays.fill(la, 0L);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            la[j] += i;
        }
        long s = 0;
        for (int k = 0; k < la.length; k++) {
            s += la[k] * (k + 1);
        }
        return (int) (s ^ (s >>> 32));
    }

    // Locals: f 0, n 1, j 2, i 3.
    static int floatInc(float[] f, int n) {
        java.util.Arrays.fill(f, 0f);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            f[j] += 1f;
        }
        double s = 0;
        for (int k = 0; k < f.length; k++) {
            s += f[k] * (k + 1);
        }
        return (int) s;
    }

    static int mixed(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] += SF;
            a[j]++;
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
        final L7W75ArrayDeclineMemoBench t = new L7W75ArrayDeclineMemoBench();
        row("staticDecl", () -> staticDecl(IA, n));
        row("fieldStatic", () -> t.fieldStatic(n));
        row("addArr", () -> addArr(IA, IB));
        row("twoLoadsMix", () -> twoLoadsMix(IA, IB));
        row("longWiden", () -> longWiden(LA, n));
        row("floatInc", () -> floatInc(FA, n));
        row("mixed", () -> mixed(IA, n));
        row("intInc", () -> intInc(IA, n));
        row("twoLoads", () -> twoLoads(IA, n));
        row("fieldArray", () -> t.fieldArray(n));
        row("arrayRead", () -> arrayRead(IA, n));
        row("localInc", () -> localInc(n));
    }
}
