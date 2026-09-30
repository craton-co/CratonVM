/**
 * QuickBenchLong2 — further-rescaled QuickBenchLong variant requested
 * 2026-07-10: Arithmetic 1.5B->2B, Fibonacci(42)->Fibonacci(44),
 * Sieve 100Kx500->100Kx20K reps, Matrix 500x500->1280x1280. Binary Trees
 * stays at depth 18 (already the standalone "bt18" case elsewhere).
 *
 * Same four kernels as QuickBench (Arithmetic, Fibonacci, Sieve, Matrix)
 * plus binary-trees at depth 18, same output shape as QuickBenchLong so
 * results diff cleanly.
 */
public class QuickBenchLong2 {

    static long benchArithmetic(long iterations) {
        long sum = 0;
        for (long i = 0; i < iterations; i++) {
            sum += i * 3 - i / 2 + i % 7;
        }
        return sum;
    }

    static int fib(int n) {
        if (n <= 1) return n;
        return fib(n - 1) + fib(n - 2);
    }

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

    // ---- binary-trees (classic GC-pressure benchmark) ----
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

    public static void main(String[] args) {
        System.out.println("=== QuickBenchLong2 ===");
        long total = 0;

        {
            long t0 = System.currentTimeMillis();
            long r = benchArithmetic(2_000_000_000L);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("1. Arithmetic (2B)      : " + elapsed + " ms  [" + r + "]");
        }
        {
            long t0 = System.currentTimeMillis();
            int r = fib(44);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("2. Fibonacci(44)        : " + elapsed + " ms  [" + r + "]");
        }
        {
            int limit = 100000;
            boolean[] composite = new boolean[limit + 1];
            int reps = 20000;
            long t0 = System.currentTimeMillis();
            int r = 0;
            for (int rep = 0; rep < reps; rep++) {
                r = sieve(composite, limit);
            }
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("3. Sieve (100Kx20K)     : " + elapsed + " ms  [" + r + "]");
        }
        {
            int n = 1280;
            int[][] a = new int[n][n];
            int[][] b = new int[n][n];
            for (int i = 0; i < n; i++) {
                for (int j = 0; j < n; j++) {
                    a[i][j] = i + j;
                    b[i][j] = i - j;
                }
            }
            long t0 = System.currentTimeMillis();
            int[][] c = matmul(a, b, n);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("4. Matrix 1280x1280     : " + elapsed + " ms  [" + c[640][640] + "]");
        }
        {
            long t0 = System.currentTimeMillis();
            long r = binaryTrees(18);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("5. Binary Trees (d=18)  : " + elapsed + " ms  [" + r + "]");
        }

        System.out.println();
        System.out.println("TOTAL                   : " + total + " ms");
    }
}
