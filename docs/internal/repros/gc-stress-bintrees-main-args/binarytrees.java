// Object-based binarytrees (canonical CLBG), accumulating single checksum.
public final class binarytrees {
    static final int minDepth = 4;

    static class TreeNode {
        TreeNode left, right;
    }

    static int itemCheck(TreeNode node) {
        if (node.left == null) return 1;
        return 1 + itemCheck(node.left) + itemCheck(node.right);
    }

    static TreeNode bottomUpTree(int depth) {
        TreeNode node = new TreeNode();
        if (depth > 0) {
            node.left = bottomUpTree(depth - 1);
            node.right = bottomUpTree(depth - 1);
        }
        return node;
    }

    public static void main(String[] args) {
        int maxDepth = Integer.parseInt(args[0]);
        if (minDepth + 2 > maxDepth) maxDepth = minDepth + 2;
        int stretchDepth = maxDepth + 1;

        long total = 0;

        int stretchTree = itemCheck(bottomUpTree(stretchDepth));
        total += stretchTree;

        TreeNode longLivedTree = bottomUpTree(maxDepth);

        for (int depth = minDepth; depth <= maxDepth; depth += 2) {
            int iterations = 1 << (maxDepth - depth + minDepth);
            int chk = 0;
            for (int i = 1; i <= iterations; i++) {
                chk += itemCheck(bottomUpTree(depth));
            }
            total += chk;
        }

        total += itemCheck(longLivedTree);

        System.out.println(total);
    }
}
