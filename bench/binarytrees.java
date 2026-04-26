/* The Computer Language Benchmarks Game
   Binary Trees - GC stress test with deep recursive tree allocation
   Uses flat int arrays to avoid needing object allocation for tree nodes.
*/
public final class binarytrees {
    static final int minDepth = 4;
    static int[] nodePool;
    static int nodeIdx;

    static int check(int[] tree, int node) {
        int left = tree[node];
        if (left == -1) return 1;
        return 1 + check(tree, left) + check(tree, tree[node+1]);
    }

    static int bottomUpTree(int depth) {
        int node = nodeIdx;
        nodeIdx += 2;
        if (depth > 0) {
            nodePool[node] = bottomUpTree(depth - 1);
            nodePool[node+1] = bottomUpTree(depth - 1);
        } else {
            nodePool[node] = -1;
            nodePool[node+1] = -1;
        }
        return node;
    }

    public static void main(String[] args) {
        int n = 18;
        int maxDepth = (minDepth + 2 > n) ? minDepth + 2 : n;
        int stretchDepth = maxDepth + 1;

        // Warmup
        for(int i=0; i<3; i++) {
            nodePool = new int[2 << (14+1)]; nodeIdx = 0;
            bottomUpTree(14);
        }

        long t0 = System.currentTimeMillis();

        nodePool = new int[2 << (stretchDepth+1)]; nodeIdx = 0;
        int stretchTree = bottomUpTree(stretchDepth);
        System.out.println("stretch tree of depth " + stretchDepth + "\t check: " + check(nodePool, stretchTree));

        int[] longLivedPool = new int[2 << (maxDepth+1)];
        nodePool = longLivedPool; nodeIdx = 0;
        int longLivedTree = bottomUpTree(maxDepth);

        for (int depth = minDepth; depth <= maxDepth; depth += 2) {
            int iterations = 1 << (maxDepth - depth + minDepth);
            int chk = 0;
            for (int i = 1; i <= iterations; i++) {
                nodePool = new int[2 << (depth+1)]; nodeIdx = 0;
                int t = bottomUpTree(depth);
                chk += check(nodePool, t);
            }
            System.out.println(iterations + "\t trees of depth " + depth + "\t check: " + chk);
        }
        nodePool = longLivedPool;
        System.out.println("long lived tree of depth " + maxDepth + "\t check: " + check(nodePool, longLivedTree));

        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("=== Binary Trees (depth="+n+") ===");
        System.out.println("Time: " + elapsed + " ms");
    }
}
