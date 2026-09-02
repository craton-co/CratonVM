// F-05 end-to-end probe: a retained old-generation tree whose leaves are
// re-pointed at freshly allocated young arrays every round, so the edges the
// remembered set has to carry are old->young and load-bearing. The checksum is
// computed from data reachable ONLY through those edges, so a lost edge shows
// up as a wrong number (or a crash), not as a slower run.
public class G1CardChurn {
    static final class Node {
        Node left;
        Node right;
        int[] payload;
    }

    static Node build(int depth) {
        Node n = new Node();
        if (depth > 0) {
            n.left = build(depth - 1);
            n.right = build(depth - 1);
        }
        return n;
    }

    static void repoint(Node n, int round) {
        if (n.left == null) {
            int[] a = new int[16];
            for (int i = 0; i < a.length; i++) {
                a[i] = round * 31 + i;
            }
            n.payload = a;
            return;
        }
        repoint(n.left, round);
        repoint(n.right, round);
    }

    static long sum(Node n) {
        if (n.left == null) {
            long s = 0;
            if (n.payload != null) {
                for (int v : n.payload) {
                    s += v;
                }
            }
            return s;
        }
        return sum(n.left) + sum(n.right);
    }

    public static void main(String[] args) {
        int depth = args.length > 0 ? Integer.parseInt(args[0]) : 12;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;

        // Four retained trees, so the holders age into Old while the arrays
        // they point at stay young.
        Node[] trees = new Node[4];
        for (int i = 0; i < trees.length; i++) {
            trees[i] = build(depth);
        }

        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < trees.length; i++) {
                repoint(trees[i], r + i);
                // Churn so a young collection actually happens between the
                // store and the read.
                for (int k = 0; k < 64; k++) {
                    int[] junk = new int[128];
                    junk[0] = k;
                    checksum += junk[0] & 1;
                }
                checksum += sum(trees[i]);
            }
        }
        System.out.println("checksum=" + checksum);
    }
}
