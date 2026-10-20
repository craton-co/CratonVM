// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L7: ns per loop iteration for
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_SCREEN  the fused array increment's
//       three entries screen the group's bytes in the dispatch loop, so a
//       declined shape (`a[j] <<= 1`) pays no call (default off);
//   CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE        `s += a[j]` in one
//       dispatch (default off; proposal
//       i72-L7-proposal-fuse-the-array-element-accumulate-20261105).
// The wave-72 array increment switches (base, VALUE, TOP) are on by
// default; DUP2 is off. The screen's question is whether DUP2 can flip:
// compare, on the decline rows, `default` (base+VALUE+TOP), `+dup2`,
// `+screen` and `+dup2+screen` against the off state
// (CRATONVM_INTERP_FUSED_ARRAY_INCREMENT=0): with the screen the decline
// rows should be within 1-2% of the off state whatever arms are on, and
// the hit rows should keep their wave-72 gain.
//
// Every row runs `j = i & 7` (or `i & 7` inline) and one group per
// iteration over an 8-element array unless said otherwise.
// Rows (1 000 000 iterations each):
//   shiftDecl     `a[j] <<= 1`: declined by the local entry, the stack-top
//                 entry (TOP) and the `dup2` entry (DUP2). SCREEN: down to
//                 the off state.
//   divDecl       `a[j] /= 3` (idiv: declined, like shiftDecl).
//   staticDecl    `a[j] += SF` (a getstatic operand: declined).
//   fieldShift    `counts[j] <<= 1` on a field array: declined by TOP and
//                 DUP2 only.
//   histShift     `h[i & 7] <<= 1`: declined by DUP2 only.
//   intInc        `a[j]++` (a hit). SCREEN: flat (the screen's cost on a hit).
//   postUsed      `last = a[j]++` (a value-form hit). SCREEN: flat.
//   fieldArray    `counts[j]++` (a TOP hit). SCREEN: flat.
//   hist          `h[i & 7]++` (a DUP2 hit). SCREEN: flat.
//   arrayRead     `s += a[j]`. ACCUMULATE: down.
//   sumDirect     `s += a[i]` over a 1000-element array, 1000 times.
//                 ACCUMULATE: down.
//   byteSum       `s += b[j]` over a byte[]. ACCUMULATE: down.
//   charXor       `s ^= c[j]` over a char[]. ACCUMULATE: down.
//   localInc      `s += j` (no array). Flat under every switch.
//   twoLoads      `a[j] = a[j] + 1` (never fused). Flat.
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the env sets on the
// SAME binary, interleaved, several rounds, and take the median of the
// per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W73ArrayDeclineAccumulateBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of 9)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// shiftDecl checksum=0
// divDecl checksum=0
// staticDecl checksum=13500000
// fieldShift checksum=0
// histShift checksum=0
// intInc checksum=4500000
// postUsed checksum=249999
// fieldArray checksum=4500000
// hist checksum=4500000
// arrayRead checksum=1000000
// sumDirect checksum=603000352
// byteSum checksum=-121500000
// charXor checksum=0
// localInc checksum=3500000
// twoLoads checksum=4500000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W73ArrayDeclineAccumulateBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static final int[] IA = new int[8];
    static final int[] BIG = new int[1000];
    static final byte[] BA = new byte[8];
    static final char[] CA = new char[8];

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

    static int shiftDecl(int[] a, int n) {
        java.util.Arrays.fill(a, 1);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] <<= 1;
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

    static int staticDecl(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] += SF;
        }
        return weigh(a);
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

    static int intInc(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j]++;
        }
        return weigh(a);
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

    int fieldArray(int n) {
        java.util.Arrays.fill(counts, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            counts[j]++;
        }
        return weigh(counts);
    }

    static int hist(int[] h, int n) {
        java.util.Arrays.fill(h, 0);
        for (int i = 0; i < n; i++) {
            h[i & 7]++;
        }
        return weigh(h);
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

    // Locals: a = 0, reps = 1, s = 2, i = 3 (`iload_2; aload_0; iload_3;
    // iaload; iadd; istore_2`), r = 4.
    static int sumDirect(int[] a, int reps) {
        int s = 0;
        int i = 0;
        for (int r = 0; r < reps; r++) {
            for (i = 0; i < a.length; i++) {
                s += a[i];
            }
        }
        return s;
    }

    static int byteSum(byte[] b, int n) {
        for (int k = 0; k < b.length; k++) {
            b[k] = (byte) (-125 + k);
        }
        int j = 0;
        int s = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            s += b[j];
        }
        return s;
    }

    static int charXor(char[] c, int n) {
        for (int k = 0; k < c.length; k++) {
            c[k] = (char) (0xfff0 + k);
        }
        int j = 0;
        int s = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            s ^= c[j];
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

    static int twoLoads(int[] a, int n) {
        java.util.Arrays.fill(a, 0);
        int j = 0;
        for (int i = 0; i < n; i++) {
            j = i & 7;
            a[j] = a[j] + 1;
        }
        return weigh(a);
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
        final L7W73ArrayDeclineAccumulateBench t = new L7W73ArrayDeclineAccumulateBench();
        for (int k = 0; k < BIG.length; k++) {
            BIG[k] = k * 7919 - 1_000_000;
        }
        row("shiftDecl", () -> shiftDecl(IA, n));
        row("divDecl", () -> divDecl(IA, n));
        row("staticDecl", () -> staticDecl(IA, n));
        row("fieldShift", () -> t.fieldShift(n));
        row("histShift", () -> histShift(IA, n));
        row("intInc", () -> intInc(IA, n));
        row("postUsed", () -> postUsed(IA, n));
        row("fieldArray", () -> t.fieldArray(n));
        row("hist", () -> hist(IA, n));
        row("arrayRead", () -> arrayRead(IA, n));
        row("sumDirect", () -> sumDirect(BIG, n / BIG.length));
        row("byteSum", () -> byteSum(BA, n));
        row("charXor", () -> charXor(CA, n));
        row("localInc", () -> localInc(n));
        row("twoLoads", () -> twoLoads(IA, n));
    }
}
