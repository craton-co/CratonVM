public final class RHard {
    static final int minDepth = 4;
    static class TreeNode { TreeNode left, right; }
    static int itemCheck(TreeNode node) {
        if (node.left == null) return 1;
        return 1 + itemCheck(node.left) + itemCheck(node.right);
    }
    static TreeNode bottomUpTree(int depth) {
        TreeNode node = new TreeNode();
        if (depth > 0) { node.left = bottomUpTree(depth - 1); node.right = bottomUpTree(depth - 1); }
        return node;
    }
    public static void main(String[] args) {
        int maxDepth = 14;
        int stretchDepth = maxDepth + 1;
        long total = 0;
        total += itemCheck(bottomUpTree(stretchDepth));
        TreeNode longLivedTree = bottomUpTree(maxDepth);
        for (int depth = minDepth; depth <= maxDepth; depth += 2) {
            int iterations = 1 << (maxDepth - depth + minDepth);
            int chk = 0;
            for (int i = 1; i <= iterations; i++) chk += itemCheck(bottomUpTree(depth));
            total += chk;
        }
        total += itemCheck(longLivedTree);
        System.out.println(total);
    }
}
