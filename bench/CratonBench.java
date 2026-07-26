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
 *                           matrix|hashmap|stringregex|bintrees)
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

    static int matrixKernel(int n) {
        int[][] a = new int[n][n];
        int[][] b = new int[n][n];
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < n; j++) {
                a[i][j] = i + j;
                b[i][j] = i - j;
            }
        }
        int[][] c = matmul(a, b, n);
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
        System.out.println("=== CratonBench ===");
        long total = 0;
        long quickbench = 0;

        if (wants(filter, "arithmetic")) {
            long t0 = System.currentTimeMillis();
            long r = benchArithmetic(2_000_000_000L);
            long ms = report("1. arithmetic  (2B ops)   ", t0, r);
            total += ms;
            quickbench += ms;
            if (all) System.gc();
        }
        if (wants(filter, "fib")) {
            long t0 = System.currentTimeMillis();
            int r = fib(44);
            long ms = report("2. fib         (44)       ", t0, r);
            total += ms;
            quickbench += ms;
            if (all) System.gc();
        }
        if (wants(filter, "sieve")) {
            long t0 = System.currentTimeMillis();
            int r = sieveReps(100_000, 20_000);
            long ms = report("3. sieve       (100Kx20K) ", t0, r);
            total += ms;
            quickbench += ms;
            if (all) System.gc();
        }
        if (wants(filter, "matrix")) {
            long t0 = System.currentTimeMillis();
            int r = matrixKernel(1280);
            long ms = report("4. matrix      (1280x1280)", t0, r);
            total += ms;
            quickbench += ms;
            if (all) System.gc();
        }
        if (all) {
            System.out.println("QUICKBENCH SUBTOTAL       : " + quickbench + " ms");
        }
        if (wants(filter, "hashmap")) {
            long t0 = System.currentTimeMillis();
            long r = hashMapPutGet(10_000_000);
            total += report("5. hashmap     (10M)      ", t0, r);
            if (all) System.gc();
        }
        if (wants(filter, "stringregex")) {
            long t0 = System.currentTimeMillis();
            long r = stringRegex(100_000);
            total += report("6. stringregex (100K)     ", t0, r);
            if (all) System.gc();
        }
        if (wants(filter, "bintrees")) {
            long t0 = System.currentTimeMillis();
            long r = binaryTrees(18);
            total += report("7. bintrees    (d=18)     ", t0, r);
        }

        System.out.println();
        System.out.println("TOTAL                     : " + total + " ms");
    }
}
