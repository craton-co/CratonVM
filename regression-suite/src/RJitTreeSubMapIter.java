/**
 * Regression: a COMPILED `for (e : treeMap.tailMap(k).entrySet())` iterated
 * ZERO entries while `entrySet().size()` on the same object answered 6.
 *
 * Root cause: guarded monomorphic virtual/interface inlining
 * (`CRATONVM_JIT_GUARDED_VIRTUAL_INLINE`) defaulted ON from a commit whose own
 * subject reads "wip(pgo-02): plumbing ... (checkpoint, no codegen yet)", while
 * three places in `jit/src/lib.rs` documented the same flag as "default-off,
 * unsoaked" and reasoned about correctness on that basis. The speculated
 * `iterator()` body was spliced against a receiver whose `root` mirror it then
 * read as null, so the iterator came back empty.
 *
 * This is `org.h2.test.store.TestRandomMapOps` op:1033, which failed H2 in
 * 11-22 s and blocked the box/unbox SIGSEGV investigation, whose crash needs
 * 25-183 s to appear.
 *
 * Three properties are load-bearing:
 *
 *  - The keys are the EXACT 42 the H2 failure held, and the bound (1810) is its
 *    bound. A tidier map did not reproduce: an earlier probe with evenly spaced
 *    keys ran 20 000 iterations clean.
 *  - It must run hot. The divergence appears around iteration 500, when the
 *    method is compiled; a handful of calls proves nothing.
 *  - `size()` is checked BESIDE the iteration every round. The defect is
 *    exactly that the two disagree, and a test that only counted entries could
 *    not tell this from an empty map.
 */
import java.util.Iterator;
import java.util.Map;
import java.util.Set;
import java.util.SortedMap;
import java.util.TreeMap;

public class RJitTreeSubMapIter {

    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) throw new AssertionError(m);
    }

    static final int[] KEYS = {
        55, 56, 168, 304, 406, 450, 520, 742, 771, 832, 915, 1001, 1235, 1413,
        1471, 1509, 1525, 1568, 1619, 1620, 1621, 1622, 1623, 1624, 1625, 1626,
        1627, 1676, 1677, 1678, 1679, 1680, 1681, 1684, 1752, 1777, 1937, 1938,
        1939, 1940, 1941, 1985
    };

    static int iterTail(TreeMap<Integer, String> m, int from) {
        SortedMap<Integer, String> sub = m.tailMap(from);
        Set<Map.Entry<Integer, String>> es = sub.entrySet();
        int n = 0;
        for (Iterator<Map.Entry<Integer, String>> it = es.iterator(); it.hasNext(); ) {
            it.next();
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
        TreeMap<Integer, String> m = new TreeMap<>();
        for (int k : KEYS) {
            m.put(k, k + "_v");
        }
        final int from = 1810;
        final int wantTail = m.tailMap(from).size();
        final int wantHead = m.headMap(from + 1).size();

        int badTail = 0, badHead = 0, badTailSize = 0, badHeadSize = 0;
        for (int i = 0; i < 4000; i++) {
            if (iterTail(m, from) != wantTail) badTail++;
            if (iterHead(m, from + 1) != wantHead) badHead++;
            // The size beside the iteration: the defect is the two disagreeing.
            if (m.tailMap(from).size() != wantTail) badTailSize++;
            if (m.headMap(from + 1).size() != wantHead) badHeadSize++;
        }

        check(wantTail == 6, "tailMap(1810) must hold 6 keys, got " + wantTail);
        check(wantHead == 36, "headMap(1811) must hold 36 keys, got " + wantHead);
        check(badTail == 0, "compiled tailMap entrySet iteration diverged on "
                + badTail + " of 4000 rounds");
        check(badHead == 0, "compiled headMap entrySet iteration diverged on "
                + badHead + " of 4000 rounds");
        check(badTailSize == 0, "tailMap size diverged " + badTailSize);
        check(badHeadSize == 0, "headMap size diverged " + badHeadSize);

        System.out.println("CK RJitTreeSubMapIter tail=" + wantTail + " head=" + wantHead);
        System.out.println("CK RJitTreeSubMapIter checks=" + checks);
        System.out.println("PASS RJitTreeSubMapIter (" + checks + " checks)");
    }
}
