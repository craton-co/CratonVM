import java.util.Comparator;
import java.util.TreeMap;

/**
 * Does a JIT-compiled `new TreeMap<>(comparator)` keep its comparator?
 *
 * H2's SelectGroups$Grouped.reset() does exactly `groupByData = new
 * TreeMap<>(session)`, where the session is the Comparator<Value>, and keys it
 * with ValueRow — which is NOT Comparable. Once org/h2/ is JIT-eligible,
 * TestMetaData.testQueryStatisticsLimit dies with
 * "ClassCastException: org.h2.value.ValueRow cannot be cast to
 * java.lang.Comparable", i.e. the map fell back to natural ordering, i.e. the
 * comparator was lost.
 */
public class TreeMapCmpProbe {

    /** Deliberately NOT Comparable — natural ordering must never be reachable. */
    static final class Key {
        final int v;
        Key(int v) { this.v = v; }
    }

    static final Comparator<Key> CMP = (a, b) -> Integer.compare(a.v, b.v);

    /** The shape under test: build the map, then use it. */
    static int build(int n) {
        TreeMap<Key, Integer> m = new TreeMap<>(CMP);
        for (int i = 0; i < n; i++) {
            m.put(new Key(i ^ 0x2a), i);
        }
        int sum = 0;
        for (Integer v : m.values()) {
            sum += v;
        }
        return sum;
    }

    /** Same, but with the comparator arriving as a parameter. */
    static int buildWithArg(Comparator<Key> cmp, int n) {
        TreeMap<Key, Integer> m = new TreeMap<>(cmp);
        for (int i = 0; i < n; i++) {
            m.put(new Key(i), i);
        }
        return m.size();
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 40000;
        int expected = build(8);
        int failures = 0;
        for (int i = 0; i < iters; i++) {
            try {
                int got = build(8);
                if (got != expected) {
                    if (failures++ < 3) {
                        System.out.println("MISMATCH at " + i + ": " + got + " != " + expected);
                    }
                }
                if (buildWithArg(CMP, 4) != 4) {
                    if (failures++ < 3) {
                        System.out.println("SIZE MISMATCH at " + i);
                    }
                }
            } catch (Throwable t) {
                if (failures++ < 3) {
                    System.out.println("THREW at iteration " + i + ": " + t);
                }
            }
        }
        // Comparator identity must survive too, not just "some ordering".
        TreeMap<Key, Integer> m = new TreeMap<>(CMP);
        if (m.comparator() != CMP) {
            failures++;
            System.out.println("COMPARATOR LOST: " + m.comparator());
        }
        System.out.println(failures == 0
                ? "PASS TreeMapCmpProbe (" + iters + " iterations)"
                : "FAIL TreeMapCmpProbe: " + failures + " failures");
        if (failures != 0) {
            System.exit(1);
        }
    }
}
