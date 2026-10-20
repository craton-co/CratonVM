// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L7: ns per loop iteration for the fused
// array element increment (proposal
// i71-L7-proposal-fuse-the-array-element-increment-20261104):
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT        `a[j]++`, `a[j] += k`, ...
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_VALUE  `x = a[j]++`, `x = ++a[j]`
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_TOP    `this.counts[j]++`,
//                                                `m[r][c]++`
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_DUP2   every group, entered at its
//                                                `dup2` (`h[i & 7]++`)
// All four default off; VALUE, TOP and DUP2 are admitted only with the base
// one. DUP2 alone (with the base) serves every row the base and TOP serve,
// two dispatches later: compare `base`, `base+top`, `base+dup2` and
// `base+top+dup2` on the int rows.
//
// Every row runs `j = i & 7` (or the matrix's two indexes) and one group per
// iteration over an 8-element array, so the rows differ only in the group.
// Rows (1 000 000 iterations each):
//   intInc       `a[j]++` int[]. BASE: down.
//   intAddK      `a[j] += k` (iload). BASE: down.
//   intSubConst  `a[j] -= 2`. BASE: down.
//   intOr        `a[j] |= k`. BASE: down.
//   longInc      `la[j]++` long[] (lconst_1; ladd). BASE: down.
//   longAddK     `la[j] += k` (lload). BASE: down.
//   byteInc      `b[j]++` byte[] (i2b). BASE: down.
//   charInc      `c[j]++` char[] (i2c). BASE: down.
//   shortInc     `s[j]++` short[] (i2s). BASE: down.
//   postUsed     `last = a[j]++` (dup_x2). VALUE: down; BASE alone: flat
//                (a declined call per iteration: must not be up much).
//   preUsed      `last = ++a[j]`. VALUE: down.
//   fieldArray   `counts[j]++` on an instance field array. TOP: down.
//   matrix       `m[r][c]++` (the row from an aaload). TOP: down.
//   twoLoads     `a[j] = a[j] + 1` (no dup2: never fused). Flat.
//   shiftDecl    `a[j] <<= 1` from 1 (shifts out to 0; an ishl group: the
//                helper declines it). BASE: must not be up much.
//   arrayRead    `s += a[j]` (the existing aload; iload; iaload fusion).
//                Flat under every switch: the off-state screen's cost.
//   localInc     `s += j` (no array). Flat.
//   hist         `h[i & 7]++` (computed index: only DUP2 fuses it). DUP2:
//                down; every other set flat.
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the env sets on the
// SAME binary, interleaved, several rounds, and take the median of the
// per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W72FusedArrayIncrementBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of 9)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// intInc checksum=4500000
// intAddK checksum=13500000
// intSubConst checksum=-9000000
// intOr checksum=180
// longInc checksum=4500000
// longAddK checksum=13500000
// byteInc checksum=2592
// charInc checksum=2140704
// shortInc checksum=-218592
// postUsed checksum=249999
// preUsed checksum=250000
// fieldArray checksum=4500000
// matrix checksum=4500000
// twoLoads checksum=4500000
// shiftDecl checksum=0
// arrayRead checksum=1000000
// localInc checksum=3500000
// hist checksum=4500000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W72FusedArrayIncrementBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static final int[] IA = new int[8];
    static final long[] LA = new long[8];
    static final byte[] BA = new byte[8];
    static final char[] CA = new char[8];
    static final short[] SA = new short[8];

    int[] counts = new int[8];
    int[][] m = new int[2][8];

    /** Sum of `a[k] * (k + 1)`. */
    static int weigh(int[] a) {
        int s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return s;
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

    static int intAddK(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        int k = 3;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] += k;
        }
        return weigh(a);
    }

    static int intSubConst(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] -= 2;
        }
        return weigh(a);
    }

    static int intOr(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        int k = 5;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] |= k;
        }
        return weigh(a);
    }

    static int weighLong(long[] a) {
        long s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return (int) s;
    }

    static int longInc(long[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j]++;
        }
        return weighLong(a);
    }

    static int longAddK(long[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        long k = 3;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] += k;
        }
        return weighLong(a);
    }

    static int byteInc(byte[] a, int n) {
        java.util.Arrays.fill(a, (byte) 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j]++;
        }
        int s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return s;
    }

    static int charInc(char[] a, int n) {
        java.util.Arrays.fill(a, (char) 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j]++;
        }
        int s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return s;
    }

    static int shortInc(short[] a, int n) {
        java.util.Arrays.fill(a, (short) 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j]++;
        }
        int s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return s;
    }

    static int postUsed(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        int last = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            last = a[j]++;
        }
        return last + a[0];
    }

    static int preUsed(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        int last = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            last = ++a[j];
        }
        return last + a[0];
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

    int matrix(int n) {
        java.util.Arrays.fill(m[0], 0);
        java.util.Arrays.fill(m[1], 0);
        int r = 0;
        int c = 0;
        for (int i = 0; i < n; i++) {
            r = i & 1;
            c = (i >> 1) & 7;
            m[r][c]++;
        }
        return weigh(m[0]) + weigh(m[1]);
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

    static int shiftDecl(int[] a, int n) {
        java.util.Arrays.fill(a, 1);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] <<= 1;
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

    static int hist(int[] h, int n) {
        java.util.Arrays.fill(h, 0);
        for (int i = 0; i < n; i++) {
            h[i & 7]++;
        }
        return weigh(h);
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
        final L7W72FusedArrayIncrementBench t = new L7W72FusedArrayIncrementBench();
        row("intInc", () -> intInc(IA, n));
        row("intAddK", () -> intAddK(IA, n));
        row("intSubConst", () -> intSubConst(IA, n));
        row("intOr", () -> intOr(IA, n));
        row("longInc", () -> longInc(LA, n));
        row("longAddK", () -> longAddK(LA, n));
        row("byteInc", () -> byteInc(BA, n));
        row("charInc", () -> charInc(CA, n));
        row("shortInc", () -> shortInc(SA, n));
        row("postUsed", () -> postUsed(IA, n));
        row("preUsed", () -> preUsed(IA, n));
        row("fieldArray", () -> t.fieldArray(n));
        row("matrix", () -> t.matrix(n));
        row("twoLoads", () -> twoLoads(IA, n));
        row("shiftDecl", () -> shiftDecl(IA, n));
        row("arrayRead", () -> arrayRead(IA, n));
        row("localInc", () -> localInc(n));
        row("hist", () -> hist(IA, n));
    }
}
