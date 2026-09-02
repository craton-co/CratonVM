/**
 * BinTreesClassic — standalone classic binary-trees benchmark (the README
 * "Binary Trees (depth=18, isolated)" row). Same kernel as QuickBenchLong2
 * kernel 5 / BenchSuite bintreesNN: stretch tree, long-lived tree, and the
 * depth 4..maxDepth iteration loop. Checksum at depth 18: 68332206.
 */
public class BinTreesClassic {
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
        int d = args.length > 0 ? Integer.parseInt(args[0]) : 18;
        long t0 = System.currentTimeMillis();
        long r = binaryTrees(d);
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("BinTrees d=" + d + ": " + elapsed + " ms  [" + r + "]");
    }
}
