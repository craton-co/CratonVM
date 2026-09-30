/**
 * SinkProbe: the single-pass tier's allocation sinking (`op_object.rs`,
 * "Allocation sinking", CRATONVM_JIT_ALLOC_SINK). `new Node(f(..), f(..))`
 * with `f` this method's own recursion allocates at the constructor call,
 * not at `new`.
 *
 *   tree   the bintrees shape; checked by node count.
 *   held   the constructed node stays on the operand stack across a later
 *          allocating call, so it must be a published root there.
 *   thrown a recursive argument call throws while the allocation is pending.
 *   deep   the recursion overflows the stack while the allocation is pending.
 *
 * prints "sinkprobe iters=N bad=B" ; bad must be 0. `held` sees a stale root
 * only when the precise roots stand alone:
 *
 *   CRATONVM_DBG_NO_JIT_ROOT_SCAN=1 CRATONVM_DBG_FORCE_MOVING=1 \
 *     cratonvm -XX:+UseGenerationalGC -Xmx64m SinkProbe
 */
public class SinkProbe {
    static final class Node {
        Node l, r;
        Node(Node l, Node r) { this.l = l; this.r = r; }
    }

    static Object[] junk = new Object[64];

    static int count(Node n) {
        return n.l == null ? 1 : 1 + count(n.l) + count(n.r);
    }

    static Node tree(int d) {
        if (d <= 0) return new Node(null, null);
        return new Node(tree(d - 1), tree(d - 1));
    }

    static int churn(int k) {
        for (int i = 0; i < junk.length; i++) junk[i] = new int[16 + (k & 7)];
        return k & 1;
    }

    static Node pick(Node n, int z) {
        return z < 0 ? null : n;
    }

    static Node held(int d) {
        if (d <= 0) return new Node(null, null);
        return pick(new Node(held(d - 1), held(d - 1)), churn(d));
    }

    static Node thrown(int d) {
        if (d == 0) throw new IllegalStateException("leaf");
        return new Node(thrown(d - 1), thrown(d - 1));
    }

    static Node deep(int d) {
        return new Node(deep(d + 1), null);
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        int bad = 0;
        for (int i = 0; i < iters; i++) {
            int d = 4 + (i & 3);
            if (count(tree(d)) != (1 << (d + 1)) - 1) bad++;
            if (count(held(d)) != (1 << (d + 1)) - 1) bad++;
            try {
                thrown(d);
                bad++;
            } catch (IllegalStateException e) {
                if (!"leaf".equals(e.getMessage())) bad++;
            }
            if ((i & 511) == 0) {
                try {
                    deep(0);
                    bad++;
                } catch (StackOverflowError e) {
                    // expected
                }
            }
        }
        System.out.println("sinkprobe iters=" + iters + " bad=" + bad);
    }
}
