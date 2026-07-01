public final class MovingYoungBtOsr {
    static final class Node {
        Node left;
        Node right;
    }

    static Node make(int depth) {
        Node n = new Node();
        if (depth > 0) {
            n.left = make(depth - 1);
            n.right = make(depth - 1);
        }
        return n;
    }

    static long check(Node n) {
        if (n.left == null) {
            return 1;
        }
        return 1 + check(n.left) + check(n.right);
    }

    static long binaryTrees(int maxDepth) {
        long total = 0;
        int minDepth = 4;
        long stretch = check(make(maxDepth + 1));
        total += stretch;
        Node longLived = make(maxDepth);
        for (int depth = minDepth; depth <= maxDepth; depth += 2) {
            int iterations = 1 << (maxDepth - depth + minDepth);
            long sum = 0;
            for (int i = 1; i <= iterations; i++) {
                sum += check(make(depth));
            }
            total += sum;
        }
        total += check(longLived);
        return total;
    }

    public static void main(String[] args) {
        int depth = args.length == 0 ? 16 : Integer.parseInt(args[0]);
        System.out.println(binaryTrees(depth));
    }
}
