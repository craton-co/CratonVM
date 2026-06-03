// Self-contained, deterministic micro-benchmark suite for cross-VM comparison.
// One benchmark per process: `java BenchSuite <name>`. Prints a single line:
//   RESULT name=<n> ms=<t> checksum=<c>
// The checksum makes correctness verifiable across VMs (must match HotSpot).
//
// ── CratonVM CPU (JIT-on) BASELINE ──────────────────────────────────────────
// Reference numbers on CratonVM's CPU path (no --gpu, JIT enabled), measured
// best-of-N to floor out machine contention. Treat these as the regression
// baseline: a rerun should match or beat them. Each is also checksum-verified.
//
//   Benchmark           name          baseline (best ms)   checksum
//   Arithmetic 1.5B     arith1500M           4 632          2812500002999999995
//   Fibonacci(44)       fib44                3 684          701408733
//   Sieve 250K x1000    sieve250k            1 414          22044
//   Matrix 800          matrix800            1 158          15359906451
//   Binary Trees d=16   bintrees16           6 154          14985902
//   Vector-add 2^28     vadd2_28           101 943          108086390654238720
//
// Hardware: Windows 11, NVIDIA RTX 2060 host, JDK 25 as --java-home. Captured
// 2026-06-02. Methodology notes: single process per benchmark (cold start +
// JIT/OSR warmup included), best-of-3 (best-of-2 for vadd2_28), 16 GB heap,
// watchdog disabled for the long vector-add. Numbers are indicative and vary
// with hardware/build; the checksums are exact and must not change.
public class BenchSuite {

    static long arithmetic(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += (long) i * 3 - i / 2 + i % 7;
        }
        return sum;
    }

    static long fib(int n) {
        if (n <= 1) return n;
        return fib(n - 1) + fib(n - 2);
    }

    static long sieve(int limit, int reps) {
        long count = 0;
        for (int r = 0; r < reps; r++) {
            boolean[] composite = new boolean[limit + 1];
            count = 0;
            for (int i = 2; i <= limit; i++) {
                if (!composite[i]) {
                    count++;
                    for (int j = i + i; j <= limit; j += i) composite[j] = true;
                }
            }
        }
        return count;
    }

    static long matmul(int n) {
        int[][] a = new int[n][n];
        int[][] b = new int[n][n];
        for (int i = 0; i < n; i++)
            for (int j = 0; j < n; j++) {
                a[i][j] = (i * 7 + j) % 13;
                b[i][j] = (i + j * 3) % 11;
            }
        int[][] c = new int[n][n];
        for (int i = 0; i < n; i++)
            for (int j = 0; j < n; j++) {
                int s = 0;
                for (int k = 0; k < n; k++) s += a[i][k] * b[k][j];
                c[i][j] = s;
            }
        long checksum = 0;
        for (int i = 0; i < n; i++)
            for (int j = 0; j < n; j++) checksum += c[i][j];
        return checksum;
    }

    // Classic CLBG binary-trees (allocation + recursion stress).
    static final class Node { Node l, r; }
    static Node make(int depth) {
        Node n = new Node();
        if (depth > 0) { n.l = make(depth - 1); n.r = make(depth - 1); }
        return n;
    }
    static long check(Node n) {
        if (n.l == null) return 1;
        return 1 + check(n.l) + check(n.r);
    }
    static long binaryTrees(int maxDepth) {
        long total = 0;
        int minDepth = 4;
        long stretch = check(make(maxDepth + 1));
        total += stretch;
        Node longLived = make(maxDepth);
        for (int depth = minDepth; depth <= maxDepth; depth += 2) {
            int iterations = 1 << (maxDepth - depth + minDepth);
            long chk = 0;
            for (int i = 1; i <= iterations; i++) chk += check(make(depth));
            total += chk;
        }
        total += check(longLived);
        return total;
    }

    // GPU-eligible shape: static counted loop over primitive arrays.
    static void vadd(int[] a, int[] b, int[] c, int n) {
        for (int i = 0; i < n; i++) c[i] = a[i] + b[i];
    }
    static long vectorAdd(int n) {
        int[] a = new int[n];
        int[] b = new int[n];
        int[] c = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = 2 * i; }
        vadd(a, b, c, n);
        long checksum = 0;
        for (int i = 0; i < n; i++) checksum += c[i];
        return checksum;
    }

    public static void main(String[] args) {
        String name = args.length > 0 ? args[0] : "all";
        long t0 = System.currentTimeMillis();
        long checksum;
        switch (name) {
            case "arith1500M": checksum = arithmetic(1_500_000_000); break;
            case "fib44":      checksum = fib(44); break;
            case "sieve250k":  checksum = sieve(250_000, 1000); break;
            case "matrix600":  checksum = matmul(600); break;
            case "matrix800":  checksum = matmul(800); break;
            case "bintrees18": checksum = binaryTrees(18); break;
            case "bintrees10": checksum = binaryTrees(10); break;
            case "bintrees12": checksum = binaryTrees(12); break;
            case "bintrees14": checksum = binaryTrees(14); break;
            case "bintrees16": checksum = binaryTrees(16); break;
            case "bintrees20": checksum = binaryTrees(20); break;
            case "vadd2_28":   checksum = vectorAdd(1 << 28); break;
            default: System.out.println("unknown bench: " + name); return;
        }
        long ms = System.currentTimeMillis() - t0;
        System.out.println("RESULT name=" + name + " ms=" + ms + " checksum=" + checksum);
    }
}
