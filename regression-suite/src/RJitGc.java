/**
 * Regression: the JIT + GC hot path — the highest-risk area of the VM. Runs
 * tight arithmetic loops (to trigger JIT compilation/OSR), heavy allocation +
 * GC churn (binary trees, to stress collection + root tracking), and a
 * megamorphic virtual-dispatch loop.
 *
 * It prints deterministic checksums; the runner diffs this output against
 * HotSpot, so the values are verified across VMs without hardcoding magic
 * numbers (any JIT/GC miscompile changes a checksum and the diff fails). The
 * in-process asserts only check true invariants (e.g. GC integrity).
 */
public class RJitGc {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    static final class Tree { Tree l, r; final int v;
        Tree(int v) { this.v = v; }
        Tree(Tree l, Tree r, int v) { this.l = l; this.r = r; this.v = v; }
    }
    static Tree make(int depth, int v) {
        if (depth == 0) return new Tree(v);
        return new Tree(make(depth - 1, 2 * v - 1), make(depth - 1, 2 * v), v);
    }
    static long checksum(Tree t) { return (t.l == null) ? t.v : t.v + checksum(t.l) - checksum(t.r); }

    interface Op { long apply(long acc, long x); }
    static final Op[] OPS = {
        (acc, x) -> acc + x, (acc, x) -> acc ^ (x << 1),
        (acc, x) -> acc - (x >> 1), (acc, x) -> acc + x * 3,
    };

    public static void main(String[] a) {
        // ---- hot arithmetic loop (int/long/float/double mix) — JIT + OSR ----
        long acc = 0; double dacc = 0;
        for (int i = 0; i < 5_000_000; i++) {
            acc += (long) i * 3 - i / 2 + i % 7;
            dacc += Math.sqrt(i & 1023);
        }
        long dbits = Double.doubleToLongBits(dacc);

        // ---- GC churn: discard many trees; keep one long-lived tree whose
        //      integrity must survive collection (computed twice, must match) ----
        Tree longLived = make(16, 12345);
        long before = checksum(longLived);
        long churn = 0;
        for (int i = 0; i < 200; i++) churn += checksum(make(12, i));
        long after = checksum(longLived);
        check(before == after, "long-lived tree corrupted by GC: " + before + " != " + after);

        // ---- megamorphic virtual dispatch ----
        long m = 0;
        for (int i = 0; i < 1_000_000; i++) m = OPS[i & 3].apply(m, i);

        // ---- array-heavy loop (bounds checks, fill/scan) ----
        int[] buf = new int[10_000];
        long fillSum = 0;
        for (int rep = 0; rep < 200; rep++) {
            for (int i = 0; i < buf.length; i++) buf[i] = (i * rep) & 0xFFFF;
            for (int v : buf) fillSum += v;
        }

        // Deterministic checksums — diffed against HotSpot by the runner.
        System.out.println("CK arith=" + acc + " sqrt=" + dbits + " churn=" + churn
                + " mega=" + m + " fill=" + fillSum + " tree=" + after);
        System.out.println("PASS RJitGc (" + checks + " checks)");
    }
}
