// Diagnostic: binaryTrees with per-component breakdown (stretch / innerSum / llCheck).
// Replicates BenchSuite.binaryTrees exactly so it OSRs + GCs the same way.
public class BTDiag {
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
        long innerSum = 0;
        for (int depth = minDepth; depth <= maxDepth; depth += 2) {
            int iterations = 1 << (maxDepth - depth + minDepth);
            long chk = 0;
            for (int i = 1; i <= iterations; i++) chk += check(make(depth));
            total += chk;
            innerSum += chk;
        }
        long llCheck = check(longLived);
        total += llCheck;
        System.out.println("stretch=" + stretch + " innerSum=" + innerSum
            + " llCheck=" + llCheck + " (golden llCheck=524287) total=" + total
            + " (golden total=67674804)");
        return total;
    }
    public static void main(String[] a) {
        binaryTrees(18);
    }
}
