/**
 * BinTreesOnly — extract just the binary-trees-d18 kernel from QuickBenchLong
 * so the orchestrator can repro the young-gen sizing bug without paying for
 * the Arithmetic / Fibonacci / Sieve / Matrix kernels first.
 *
 * Identical logic to QuickBenchLong#binaryTrees(18). One pass, prints
 * the checksum and elapsed millis.
 */
public class BinTreesOnly {

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
        int depth = 18;
        if (args.length > 0) {
            try { depth = Integer.parseInt(args[0]); } catch (NumberFormatException e) { /* keep default */ }
        }
        long t0 = System.currentTimeMillis();
        long r = binaryTrees(depth);
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("Binary Trees (d=" + depth + ") : " + elapsed + " ms  [" + r + "]");
    }
}
