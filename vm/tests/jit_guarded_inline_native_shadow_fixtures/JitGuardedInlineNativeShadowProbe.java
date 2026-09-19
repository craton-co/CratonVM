// Regression fixture for
// `internal/fixed-bugs/guarded-inline-native-screen-asked-the-declaring-class-FIXED-20260904.md`.
//
// A compiled `for (e : treeMap.tailMap(k).entrySet())` iterated ZERO entries
// while `entrySet().size()` on the same object answered 6, deterministically
// from around iteration 500 -- the point at which `iterTail` compiles.
// `org.h2.test.store.TestRandomMapOps` op:1033, reduced.
//
// Three properties are load-bearing:
//
//  * The keys are the EXACT 42 the H2 failure held, and the bound (1810) is
//    its bound. A tidier map does not reproduce: an evenly-spaced probe ran
//    20 000 iterations clean.
//  * It must run HOT. A handful of calls proves nothing; the wrong answer only
//    ever came out of a compiled body.
//  * `size()` is asked BESIDE the iteration. The defect is exactly that the
//    two disagree, and a check that only counted entries could not tell this
//    from an empty map.
//
// The `EMPTY-ITER` diagnostic below is deliberate. Without it the method fails
// exactly ONCE, on the first entry to the freshly compiled body; with it the
// extra cold-path code stops whatever repaired it and the failure persists.
// Either shape is caught here, but the persistent one is the louder signal --
// so do not "tidy" the diagnostic away, and do not read its count as severity.
import java.util.Map;
import java.util.SortedMap;
import java.util.TreeMap;

public class JitGuardedInlineNativeShadowProbe {

    static final int[] KEYS = {
        55, 56, 168, 304, 406, 450, 520, 742, 771, 832, 915, 1001, 1235, 1413,
        1471, 1509, 1525, 1568, 1619, 1620, 1621, 1622, 1623, 1624, 1625, 1626,
        1627, 1676, 1677, 1678, 1679, 1680, 1681, 1684, 1752, 1777, 1937, 1938,
        1939, 1940, 1941, 1985
    };

    static int reported = 0;

    static int iterTail(TreeMap<Integer, String> m, int from) {
        SortedMap<Integer, String> sub = m.tailMap(from);
        int n = 0;
        for (Map.Entry<Integer, String> e : sub.entrySet()) {
            n++;
        }
        if (n == 0 && reported++ < 3) {
            // Is the VIEW empty, or only the ITERATION? Asked here, inside the
            // compiled body, on a call that gets it wrong. The answer that
            // named the defect was that the view is fine and only the walk is
            // empty.
            System.out.println("EMPTY-ITER"
                    + " sub.size=" + sub.size()
                    + " sub.isEmpty=" + sub.isEmpty()
                    + " subCls=" + sub.getClass().getName()
                    + " esSize=" + sub.entrySet().size()
                    + " esCls=" + sub.entrySet().getClass().getName()
                    + " mSize=" + m.size());
        }
        return n;
    }

    static int iterHead(TreeMap<Integer, String> m, int to) {
        int n = 0;
        for (Map.Entry<Integer, String> e : m.headMap(to).entrySet()) {
            n++;
        }
        return n;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        TreeMap<Integer, String> m = new TreeMap<>();
        for (int k : KEYS) {
            m.put(k, k + "_v");
        }
        final int from = 1810;

        int wantTail = m.tailMap(from).size();
        int wantHead = m.headMap(from + 1).size();
        System.out.println("setup size=" + m.size()
                + " tailSize=" + wantTail + " headSize=" + wantHead);

        int badTail = 0, badHead = 0, firstBad = -1;
        for (int i = 0; i < iters; i++) {
            int gotTail = iterTail(m, from);
            int gotHead = iterHead(m, from + 1);
            if (gotTail != wantTail) {
                badTail++;
                if (firstBad < 0) {
                    firstBad = i;
                    System.out.println("FIRST tail divergence iter=" + i
                            + " got=" + gotTail + " want=" + wantTail
                            + " size()=" + m.tailMap(from).size());
                }
            }
            if (gotHead != wantHead) {
                badHead++;
                if (firstBad < 0) {
                    firstBad = i;
                    System.out.println("FIRST head divergence iter=" + i
                            + " got=" + gotHead + " want=" + wantHead);
                }
            }
        }
        if (badTail == 0 && badHead == 0) {
            System.out.println("PASS JitGuardedInlineNativeShadowProbe iters=" + iters);
        } else {
            System.out.println("FAIL JitGuardedInlineNativeShadowProbe iters=" + iters
                    + " badTail=" + badTail + " badHead=" + badHead
                    + " firstBad=" + firstBad);
        }
    }
}
