// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L7: ns per loop iteration for
//   CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET  a `<clinit>` loop over
//       `T[j] += SF` (the static site never fills inside a <clinit>, so with
//       the static operand on every iteration pays the call, the decode and
//       the probe; under the budget the pc retires after 64 misses)
//       (proposal i76-L7-proposal-a-miss-budget-for-the-deferred-operands-
//       20261109; default off)
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND_WIDE  the element
//       operand from `aload n` / `iload n` and with a constant index
//       expression (proposal i76-L7-proposal-wide-and-expression-operands-of-
//       the-array-increment-20261109; default off)
// and, cross-binary against wave 76, the wave-77 loop refactor (the ldc /
// static inline gates as one hoisted word): the control rows must stay flat.
//
// Rows (1 000 000 iterations each, `j = i & 7` over 8 elements unless said):
//   clinitStatic `T[j] += SF` inside a `static {}` (a fresh nested class per
//                repetition, timed around its first use). A `<clinit>` frame
//                is built by `interpreter::execute` (an owned frame, no
//                decline memo), so MISS_BUDGET cannot retire it: expected
//                flat under the budget; STATIC_OPERAND=0 vs 1 prices the miss.
//   clinitHelper the same loop in an ordinary method its `<clinit>` calls
//                (the class is not initialised yet, so the site never
//                fills). MISS_BUDGET: back to the time with STATIC_OPERAND=0.
//   wideArr      `acc[j] += x[j]`, x in local 4 (`aload 4`). WIDE: a hit.
//   wideIdx      `a[j] += b[k]`, k in local 4 (`iload 4`). WIDE: a hit.
//   offset       `h[j] += w[j + 1]`. WIDE: a hit.
//   prefix       `p[j] += p[j - 1]`, j alternating 1, 2. WIDE: a hit.
//   addArr       `a[j] += b[j]` (the wave-76 short form, a hit). Flat.
//   staticDecl   `a[j] += SF` in a plain method (a hit). Flat.
//   intInc       `a[j]++` (a hit). Flat.
//   ldcStatic    `s += K` with a non-final static K and an ldc bound (the
//                getstatic / ldc inline arms the wave-77 refactor touched).
//                Flat.
//   localInc     `s += j` (no array). Flat.
//
// Robust to a loaded host: every row except clinitStatic runs REPS = 9 timed
// repetitions after one untimed warm-up and reports the MINIMUM;
// clinitStatic runs 6 timed classes after one untimed one, clinitHelper 4. Compare env sets
// on the SAME binary, interleaved, several rounds, and take the median of the
// per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W77ArrayOperandBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of N)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// clinitStatic checksum=13500000
// clinitHelper checksum=13500000
// wideArr checksum=4500000
// wideIdx checksum=4500000
// offset checksum=4500000
// prefix checksum=1339595249
// addArr checksum=4500000
// staticDecl checksum=13500000
// intInc checksum=4500000
// ldcStatic checksum=3000000
// localInc checksum=3500000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W77ArrayOperandBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 1_000_000;

    static final int[] IA = new int[8];
    static final int[] IB = new int[8];
    static final int[] W9 = new int[9];

    static int SF = 3;
    static int K = 3;

    /** Sum of `a[k] * (k + 1)`. */
    static int weigh(int[] a) {
        int s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return s;
    }

    // Seven copies of one `<clinit>` loop: each class initialises once, so
    // each repetition needs its own. Locals in each <clinit>: j 0, i 1
    // (`getstatic T; iload_0; dup2; iaload; getstatic SF; iadd; iastore`, the
    // stack-top entry).
    static final class C0 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM;

        static {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            SUM = weigh(T);
        }
    }

    static final class C1 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM;

        static {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            SUM = weigh(T);
        }
    }

    static final class C2 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM;

        static {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            SUM = weigh(T);
        }
    }

    static final class C3 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM;

        static {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            SUM = weigh(T);
        }
    }

    static final class C4 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM;

        static {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            SUM = weigh(T);
        }
    }

    static final class C5 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM;

        static {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            SUM = weigh(T);
        }
    }

    static final class C6 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM;

        static {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            SUM = weigh(T);
        }
    }

    // Five copies of a `<clinit>` that calls a helper: the helper is an
    // ordinary method (a frame that keeps the decline memo), but its class is
    // not initialised until the helper returns, so its `getstatic SF` site
    // never fills while it runs. Locals in `run`: j 0, i 1.
    static final class H0 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM = run();

        static int run() {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            return weigh(T);
        }
    }

    static final class H1 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM = run();

        static int run() {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            return weigh(T);
        }
    }

    static final class H2 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM = run();

        static int run() {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            return weigh(T);
        }
    }

    static final class H3 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM = run();

        static int run() {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            return weigh(T);
        }
    }

    static final class H4 {
        static int SF = 3;
        static final int[] T = new int[8];
        static final int SUM = run();

        static int run() {
            int j = 0;
            for (int i = 0; i < ITERS; i++) {
                j = i & 7;
                T[j] += SF;
            }
            return weigh(T);
        }
    }

    /** The first use of helper class `k`, which runs its `<clinit>`. */
    static int initHelperClass(int k) {
        switch (k) {
            case 0:
                return H0.SUM;
            case 1:
                return H1.SUM;
            case 2:
                return H2.SUM;
            case 3:
                return H3.SUM;
            default:
                return H4.SUM;
        }
    }

    static void clinitHelperRow() {
        int checksum = initHelperClass(0); // untimed
        long best = Long.MAX_VALUE;
        for (int k = 1; k <= 4; k++) {
            long t0 = System.nanoTime();
            int c = initHelperClass(k);
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println("clinitHelper checksum=" + checksum);
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", "clinitHelper", (double) best / ITERS, 4);
    }

    /** The first use of class `k`, which runs its `<clinit>`. */
    static int initClass(int k) {
        switch (k) {
            case 0:
                return C0.SUM;
            case 1:
                return C1.SUM;
            case 2:
                return C2.SUM;
            case 3:
                return C3.SUM;
            case 4:
                return C4.SUM;
            case 5:
                return C5.SUM;
            default:
                return C6.SUM;
        }
    }

    static void clinitRow() {
        int checksum = initClass(0); // untimed
        long best = Long.MAX_VALUE;
        for (int k = 1; k <= 6; k++) {
            long t0 = System.nanoTime();
            int c = initClass(k);
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println("clinitStatic checksum=" + checksum);
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", "clinitStatic", (double) best / ITERS, 6);
    }

    // Locals: acc 0, j 1, pad 2, pad2 3, x 4, i 5. The group opens with
    // `aload_0; iload_1` (the local entry); its operand is `aload 4; iload_1;
    // iaload`.
    static int wideArr(int[] acc, int j, int pad, int pad2, int[] x) {
        java.util.Arrays.fill(acc, 0);
        java.util.Arrays.fill(x, 1);
        for (int i = 0; i < ITERS; i++) {
            j = i & 7;
            acc[j] += x[j];
        }
        return weigh(acc);
    }

    // Locals: a 0, b 1, j 2, pad 3, k 4, i 5. The operand is `aload_1;
    // iload 4; iaload`.
    static int wideIdx(int[] a, int[] b, int j, int pad, int k) {
        java.util.Arrays.fill(a, 0);
        java.util.Arrays.fill(b, 1);
        for (int i = 0; i < ITERS; i++) {
            j = i & 7;
            k = j;
            a[j] += b[k];
        }
        return weigh(a);
    }

    // Locals: h 0, w 1, j 2, i 3. The operand is `aload_1; iload_2;
    // iconst_1; iadd; iaload`.
    static int offset(int[] h, int[] w) {
        java.util.Arrays.fill(h, 0);
        java.util.Arrays.fill(w, 1);
        int j = 0;
        for (int i = 0; i < ITERS; i++) {
            j = i & 7;
            h[j] += w[j + 1];
        }
        return weigh(h);
    }

    // Locals: p 0, n 1, j 2, i 3. `p[j] += p[j - 1]` with j = 1, 2, 1, 2...
    // from p = {1, 0, ...}: p[1] ends at n / 2 and p[2] at the triangular
    // number of n / 2 (wrapped to int).
    static int prefix(int[] p, int n) {
        java.util.Arrays.fill(p, 0);
        p[0] = 1;
        int j = 1;
        for (int i = 0; i < n; i++) {
            j = (i & 1) + 1;
            p[j] += p[j - 1];
        }
        return weigh(p);
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

    // `s += K` (a quickened getstatic) under an `ldc` loop bound past
    // sipush's range (the fused ldc bound).
    static int ldcStatic() {
        int s = 0;
        for (int i = 0; i < 1_000_000; i++) {
            s += K;
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
        clinitRow();
        clinitHelperRow();
        row("wideArr", () -> wideArr(IA, 0, 0, 0, IB));
        row("wideIdx", () -> wideIdx(IA, IB, 0, 0, 0));
        row("offset", () -> offset(IA, W9));
        row("prefix", () -> prefix(IA, n));
        row("addArr", () -> addArr(IA, IB));
        row("staticDecl", () -> staticDecl(IA, n));
        row("intInc", () -> intInc(IA, n));
        row("ldcStatic", L7W77ArrayOperandBench::ldcStatic);
        row("localInc", () -> localInc(n));
    }
}
