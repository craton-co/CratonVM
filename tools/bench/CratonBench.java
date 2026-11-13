import java.util.HashMap;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * CratonBench — the unified successor to the scattered QuickBenchLong2 /
 * HashMapOnly / StringRegexOnly / BinTreesClassic set ("QuickBench" only
 * ever named the first four kernels; the other rows lived in one-off
 * files). One harness, seven phases, in the README table's row order:
 *
 *   1. arithmetic   — 2,000,000,000 long ops
 *   2. fib          — recursive Fibonacci(44)
 *   3. sieve        — 100,000-limit sieve x 20,000 reps
 *   4. matrix       — 1280x1280 int matmul
 *      (QUICKBENCH SUBTOTAL printed here — comparable to the historical
 *       "QuickBench TOTAL" row)
 *   5. hashmap      — 10,000,000 HashMap put/get
 *   6. stringregex  — build "1 2 .. 100000", Pattern/Matcher sum
 *   7. bintrees     — classic binary-trees GC benchmark, depth 18
 *
 * Output shape is one line per phase: "<name> : <ms> ms  [<checksum>]" so
 * runs diff cleanly and a checksum drift is as loud as a time drift.
 *
 * Usage:
 *   CratonBench             run all seven phases in-process
 *   CratonBench <phase>     run ONE phase (isolated-process methodology,
 *                           matching how the historical per-row numbers
 *                           were measured; phase = arithmetic|fib|sieve|
 *                           matrix|matrix-tiled|hashmap|stringregex|bintrees)
 *
 * Methodology notes:
 *   - Every kernel lives in a static method, NOT main: main contains
 *     invokedynamic string concat, which bails the whole-method OSR
 *     artifact compile and would silently run a main-resident loop
 *     interpreted forever (found 2026-07-17 — see HashMapOnly.java).
 *   - System.gc() runs between phases in all-phase mode to limit
 *     cross-phase heap pollution, but an all-phase run is still NOT
 *     identical methodology to seven isolated runs (JIT/GC state carries
 *     over); compare all-phase numbers to all-phase baselines and
 *     isolated to isolated.
 */
public class CratonBench {
    private static final int MATRIX_TILE = 64;

    // ---- 1. arithmetic ------------------------------------------------
    static long benchArithmetic(long iterations) {
        long sum = 0;
        for (long i = 0; i < iterations; i++) {
            sum += i * 3 - i / 2 + i % 7;
        }
        return sum;
    }

    // ---- 2. fib -------------------------------------------------------
    static int fib(int n) {
        if (n <= 1) return n;
        return fib(n - 1) + fib(n - 2);
    }

    // ---- 3. sieve -----------------------------------------------------
    static int sieve(boolean[] composite, int limit) {
        for (int i = 0; i <= limit; i++) {
            composite[i] = false;
        }
        int count = 0;
        for (int i = 2; i <= limit; i++) {
            if (!composite[i]) {
                count++;
                for (int j = i + i; j <= limit; j += i) {
                    composite[j] = true;
                }
            }
        }
        return count;
    }

    static int sieveReps(int limit, int reps) {
        boolean[] composite = new boolean[limit + 1];
        int r = 0;
        for (int rep = 0; rep < reps; rep++) {
            r = sieve(composite, limit);
        }
        return r;
    }

    // ---- 4. matrix ----------------------------------------------------
    static int[][] matmul(int[][] a, int[][] b, int n) {
        int[][] c = new int[n][n];
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                int sum = 0;
                for (int k = 0; k < n; k++) {
                    sum += a[i][k] * b[k][j];
                }
                c[i][j] = sum;
            }
        }
        return c;
    }

    // Bound each tile working set so contiguous column updates reuse cached rows.
    static int[][] matmulTiled(int[][] a, int[][] b, int n) {
        int[][] c = new int[n][n];
        for (int ii = 0; ii < n; ii += MATRIX_TILE)
            for (int kk = 0; kk < n; kk += MATRIX_TILE)
                for (int jj = 0; jj < n; jj += MATRIX_TILE) {
                    int imax = Math.min(ii + MATRIX_TILE, n);
                    int kmax = Math.min(kk + MATRIX_TILE, n);
                    int jmax = Math.min(jj + MATRIX_TILE, n);
                    for (int i = ii; i < imax; i++) {
                        int[] ai = a[i], ci = c[i];
                        for (int k = kk; k < kmax; k++) {
                            int aik = ai[k];
                            int[] bk = b[k];
                            for (int j = jj; j < jmax; j++)
                                ci[j] += aik * bk[j];
                        }
                    }
                }
        return c;
    }

    static int matrixKernel(int n) {
        return matrixKernel(n, false);
    }

    static int matrixKernel(int n, boolean tiled) {
        int[][] a = new int[n][n];
        int[][] b = new int[n][n];
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                a[i][j] = i + j;
                b[i][j] = i - j;
            }
        }
        int[][] c = tiled ? matmulTiled(a, b, n) : matmul(a, b, n);
        return c[n / 2][n / 2];
    }

    // ---- 5. hashmap ---------------------------------------------------
    static long hashMapPutGet(int n) {
        HashMap<Integer, Integer> map = new HashMap<>();
        for (int i = 0; i < n; i++) {
            map.put(i, i * 31 + 7);
        }
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += map.get(i);
        }
        return sum;
    }

    // ---- 6. stringregex -----------------------------------------------
    static long stringRegex(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 1; i <= n; i++) {
            sb.append(i).append(' ');
        }
        String s = sb.toString();
        Pattern p = Pattern.compile("(\\d+)");
        Matcher m = p.matcher(s);
        long sum = 0;
        while (m.find()) {
            sum += Long.parseLong(m.group(1));
        }
        return sum;
    }

    // ---- 7. bintrees --------------------------------------------------
    static final class Node {
        Node left, right;
        Node(Node left, Node right) { this.left = left; this.right = right; }
    }

    static int itemCheck(Node n) {
        if (n.left == null) return 1;
        return 1 + itemCheck(n.left) + itemCheck(n.right);
    }

    static Node bottomUpTree(int depth) {
        if (depth <= 0) return new Node(null, null);
        return new Node(bottomUpTree(depth - 1), bottomUpTree(depth - 1));
    }

    static long binaryTrees(int maxDepth) {
        int minDepth = 4;
        if (maxDepth < minDepth + 2) maxDepth = minDepth + 2;
        long check = 0;

        int stretchDepth = maxDepth + 1;
        check += itemCheck(bottomUpTree(stretchDepth));

        Node longLived = bottomUpTree(maxDepth);
        for (int depth = minDepth; depth <= maxDepth; depth += 2) {
            int iterations = 1 << (maxDepth - depth + minDepth);
            long sum = 0;
            for (int i = 1; i <= iterations; i++) {
                sum += itemCheck(bottomUpTree(depth));
            }
            check += sum;
        }
        check += itemCheck(longLived);
        return check;
    }

    // ---- harness ------------------------------------------------------

    static long report(String label, long t0, long checksum) {
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println(label + ": " + elapsed + " ms  [" + checksum + "]");
        return elapsed;
    }

    static boolean wants(String filter, String phase) {
        return filter == null || filter.equals(phase);
    }

    public static void main(String[] args) {
        String filter = args.length > 0 ? args[0] : null;
        boolean all = filter == null;
        if (args.length > 1 && (all || args.length > 3))
            throw new IllegalArgumentException("Usage: CratonBench [phase [size [sieve-repetitions]]]");
        long size = args.length > 1 ? Long.parseLong(args[1]) : -1;
        int repetitions = args.length > 2 ? Integer.parseInt(args[2]) : 20_000;
        if (size == 0 || size < -1 || repetitions <= 0 ||
                (size > Integer.MAX_VALUE && !"arithmetic".equals(filter)) ||
                (args.length > 2 && !"sieve".equals(filter)))
            throw new IllegalArgumentException("Invalid benchmark size or repetitions");
        System.out.println("=== CratonBench ===");
        long total = 0;
        long quickbench = 0;

        if (wants(filter, "arithmetic")) {
            long t0 = System.currentTimeMillis();
            long r = benchArithmetic(size < 0 ? 2_000_000_000L : size);
            long ms = report(size < 0 ? "1. arithmetic  (2B ops)   " : "1. arithmetic  (" + size + " ops)", t0, r);
            total += ms;
            quickbench += ms;
            if (all) System.gc();
        }
        if (wants(filter, "fib")) {
            long t0 = System.currentTimeMillis();
            int r = fib(size < 0 ? 44 : (int) size);
            long ms = report(size < 0 ? "2. fib         (44)       " : "2. fib         (" + size + ")", t0, r);
            total += ms;
            quickbench += ms;
            if (all) System.gc();
        }
        if (wants(filter, "sieve")) {
            long t0 = System.currentTimeMillis();
            int r = sieveReps(size < 0 ? 100_000 : (int) size, repetitions);
            long ms = report(size < 0 ? "3. sieve       (100Kx20K) " : "3. sieve       (" + size + "x" + repetitions + ")", t0, r);
            total += ms;
            quickbench += ms;
            if (all) System.gc();
        }
        if (wants(filter, "matrix")) {
            long t0 = System.currentTimeMillis();
            int r = matrixKernel(size < 0 ? 1280 : (int) size);
            long ms = report(size < 0 ? "4. matrix      (1280x1280)" : "4. matrix      (" + size + "x" + size + ")", t0, r);
            total += ms;
            quickbench += ms;
            if (all) System.gc();
        }
        if (wants(filter, "matrix-tiled") && !all) {
            long t0 = System.currentTimeMillis();
            int r = matrixKernel(size < 0 ? 1280 : (int) size, true);
            total += report(size < 0 ? "4b. matrix-tiled (1280x1280)" : "4b. matrix-tiled (" + size + "x" + size + ")", t0, r);
        }
        if (all) {
            System.out.println("QUICKBENCH SUBTOTAL       : " + quickbench + " ms");
        }
        if (wants(filter, "hashmap")) {
            long t0 = System.currentTimeMillis();
            long r = hashMapPutGet(size < 0 ? 10_000_000 : (int) size);
            total += report(size < 0 ? "5. hashmap     (10M)      " : "5. hashmap     (" + size + ")", t0, r);
            if (all) System.gc();
        }
        if (wants(filter, "stringregex")) {
            long t0 = System.currentTimeMillis();
            long r = stringRegex(size < 0 ? 100_000 : (int) size);
            total += report(size < 0 ? "6. stringregex (100K)     " : "6. stringregex (" + size + ")", t0, r);
            if (all) System.gc();
        }
        if (wants(filter, "bintrees")) {
            long t0 = System.currentTimeMillis();
            long r = binaryTrees(size < 0 ? 18 : (int) size);
            total += report(size < 0 ? "7. bintrees    (d=18)     " : "7. bintrees    (d=" + size + ")", t0, r);
        }

        System.out.println();
        System.out.println("TOTAL                     : " + total + " ms");
    }
}
