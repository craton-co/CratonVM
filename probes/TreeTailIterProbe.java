// TreeMap.tailMap(k).entrySet() iterates ZERO entries on a map whose
// tailMap(k).size() is 6.
//
// Reduced from H2 TestRandomMapOps op:1033. At the failure the reference
// TreeMap held these 42 keys, and for from=1810 the VM answered:
//   ceilingKey(1810) = 1937   (correct)
//   higherKey(1809)  = 1937   (correct)
//   tailMap(1810).size() = 6  (correct)
//   for (e : tailMap(1810).entrySet()) -> 0 iterations   (WRONG)
//
// So the bounds computation is right and the sub-map ITERATOR is not.
// `NavigableSubMap.absLowest()` filters its start through `tooHigh(e)`, which
// for a tailMap (toEnd = true) must return false without looking at anything.
import java.util.Map;
import java.util.TreeMap;

public class TreeTailIterProbe {

    static final int[] KEYS = {
        55, 56, 168, 304, 406, 450, 520, 742, 771, 832, 915, 1001, 1235, 1413,
        1471, 1509, 1525, 1568, 1619, 1620, 1621, 1622, 1623, 1624, 1625, 1626,
        1627, 1676, 1677, 1678, 1679, 1680, 1681, 1684, 1752, 1777, 1937, 1938,
        1939, 1940, 1941, 1985
    };

    static int iterTail(TreeMap<Integer, String> m, int from) {
        java.util.SortedMap<Integer, String> sub = m.tailMap(from);
        int n = 0;
        for (Map.Entry<Integer, String> e : sub.entrySet()) {
            n++;
        }
        if (n == 0) {
            // Is the VIEW empty, or only the ITERATION? Asked here, inside the
            // compiled body, on the one call that gets it wrong. The answer is
            // that the view is fine and re-iterating it right here gives 6.
            System.out.println("EMPTY-ITER"
                    + " sub.size=" + sub.size()
                    + " sub.isEmpty=" + sub.isEmpty()
                    + " subCls=" + sub.getClass().getName()
                    + " esSize=" + sub.entrySet().size()
                    + " esCls=" + sub.entrySet().getClass().getName()
                    + " mSize=" + m.size()
                    + " reIter=" + reCount(sub));
            System.out.flush();
        }
        return n;
    }

    static int reCount(java.util.SortedMap<Integer, String> sub) {
        int n = 0;
        for (Map.Entry<Integer, String> e : sub.entrySet()) {
            n++;
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
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300000;
        TreeMap<Integer, String> m = new TreeMap<>();
        for (int k : KEYS) {
            m.put(k, k + "_v");
        }
        final int from = 1810;

        int wantTail = m.tailMap(from).size();
        int wantHead = m.headMap(from + 1).size();
        System.out.println("setup size=" + m.size()
                + " tailSize=" + wantTail + " headSize=" + wantHead
                + " ceil=" + m.ceilingKey(from));

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
        System.out.println("TreeTailIterProbe iters=" + iters
                + " badTail=" + badTail + " badHead=" + badHead
                + " firstBad=" + firstBad);
    }
}
