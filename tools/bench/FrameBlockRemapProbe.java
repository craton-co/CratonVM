/**
 * FrameBlockRemapProbe: `hold` allocates nothing itself (so the optimizing
 * tier takes it) and keeps two reference parameters across a call whose
 * callee allocates. After the call it compares its parameter homes against statics
 * that the collector rewrites through an independent root: a home the
 * collector failed to rewrite still names the from-space copy, and the
 * comparison fails.
 *
 * prints "frameblockremap iters=N bad=B sum=S" ; bad must be 0.
 *
 * Under the shipped configuration this is green whether or not the rewrite
 * works: the conservative JIT-frame scan pins what the homes name and no
 * collector moves it. It sees the IR frame block's indirect remap only with
 * the precise roots standing alone:
 *
 *   CRATONVM_DBG_NO_JIT_ROOT_SCAN=1 CRATONVM_DBG_FORCE_MOVING=1 \
 *     cratonvm -XX:+UseGenerationalGC -Xmx64m FrameBlockRemapProbe
 *
 * (both flags are diagnostic and unsound). `hold` must reach the IR tier —
 * check for `full/ir` under CRATONVM_DBG=jit-disasm
 * CRATONVM_DBG_JIT_DISASM=FrameBlockRemapProbe.hold. See
 * docs/known-issues/perf/perf-bintrees-gap-characterised.md.
 */
public class FrameBlockRemapProbe {
    static final class Node {
        Node left, right; int v;
        Node(Node l, Node r, int v) { left = l; right = r; this.v = v; }
    }

    static Node G1, G2, sink;

    static int churn(int k) {
        Node t = null;
        for (int i = 0; i < 48; i++) t = new Node(t, null, i + k);
        sink = t;
        return t.v & 3;
    }

    static int hold(Node a, Node b, int k) {
        int r = churn(k);
        int bad = 0;
        if (a != G1) bad++;
        if (b != G2) bad++;
        if (a.left != b) bad++;
        if (b.right != a) bad++;
        r += churn(k + 1);
        if (a != G1 || b != G2) bad++;
        return r + bad * 1_000_000 + a.v + b.v;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 400000;
        long sum = 0;
        int bad = 0;
        for (int i = 0; i < iters; i++) {
            Node a = new Node(null, null, i & 1023);
            Node b = new Node(null, a, 5);
            a.left = b;
            G1 = a;
            G2 = b;
            int r = hold(a, b, i);
            bad += r / 1_000_000;
            sum += r % 1_000_000;
        }
        System.out.println("frameblockremap iters=" + iters + " bad=" + bad + " sum=" + sum);
    }
}
