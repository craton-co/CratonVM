/**
 * CtorStoreProbe: reference stores inside inlined constructors, where the
 * single-pass tier may skip a null store or drop the old-value test because
 * the field provably still holds null (`inline_ctor_store_starts_from_null`,
 * CRATONVM_JIT_CTOR_STORE_FROM_NULL).
 *
 * Each constructor below is a shape that proof must hold for or must refuse:
 *
 *   Leaf / Pair  `this.f = <param>`, the proven shape (null and non-null).
 *   Steal        `other.left = null` — not `this`; the store must happen.
 *   Twice        a second store to the same field starts from the first.
 *
 * `run` holds a small working set live across allocation churn, so the
 * collector moves objects between stores.
 *
 * prints "ctorstore iters=N bad=B sum=S" ; bad must be 0.
 */
public class CtorStoreProbe {
    static final class Node {
        Node left, right;
        Node(Node l, Node r) { left = l; right = r; }
    }

    static final class Steal {
        Node got;
        Steal(Node other) { other.left = null; got = other; }
    }

    static final class Twice {
        Node a;
        Twice(Node x) { a = x; a = null; }
    }

    static Node sink;

    static int run(int i) {
        int bad = 0;
        Node leaf = new Node(null, null);
        if (leaf.left != null || leaf.right != null) bad++;
        Node pair = new Node(leaf, null);
        if (pair.left != leaf || pair.right != null) bad++;
        Node both = new Node(leaf, pair);
        if (both.left != leaf || both.right != pair) bad++;

        Node victim = new Node(pair, pair);
        Steal s = new Steal(victim);
        if (victim.left != null || victim.right != pair || s.got != victim) bad++;

        Twice t = new Twice(both);
        if (t.a != null) bad++;

        // Churn so the next iteration's stores see a collector that has moved.
        Node c = null;
        for (int k = 0; k < 16; k++) c = new Node(c, both);
        sink = c;
        if (both.left != leaf || both.right != pair || pair.left != leaf) bad++;
        return bad;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int bad = 0;
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            int b = run(i);
            bad += b;
            sum += b == 0 ? 1 : 0;
        }
        System.out.println("ctorstore iters=" + iters + " bad=" + bad + " sum=" + sum);
    }
}
