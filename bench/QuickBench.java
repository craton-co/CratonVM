/**
 * Benchmark — each test targets ~1000ms+ for stable timing.
 */
public class QuickBench {
    static long benchArithmetic(int iterations) {
        long sum = 0;
        for (int i = 0; i < iterations; i++) {
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

    public static void main(String[] args) {
        System.out.println("=== Benchmark ===");
        long total = 0;

        {
            long t0 = System.currentTimeMillis();
            long r = benchArithmetic(300000000);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("1. Arithmetic (300M): " + elapsed + " ms  [" + r + "]");
        }
        {
            long t0 = System.currentTimeMillis();
            int r = fib(42);
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("2. Fibonacci(42)    : " + elapsed + " ms  [" + r + "]");
        }
        {
            int limit = 100000;
            boolean[] composite = new boolean[limit + 1];
            int reps = 500;
            long t0 = System.currentTimeMillis();
            int r = 0;
            for (int rep = 0; rep < reps; rep++) {
                r = sieve(composite, limit);
            }
            long elapsed = System.currentTimeMillis() - t0;
            total += elapsed;
            System.out.println("3. Sieve (100Kx500) : " + elapsed + " ms  [" + r + "]");
        }
        {
            int n = 500;
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
            System.out.println("4. Matrix 500x500   : " + elapsed + " ms  [" + c[250][250] + "]");
        }

        System.out.println();
        System.out.println("TOTAL               : " + total + " ms");
    }
}
