import java.util.*;

/** L3 residual 6.1 — the four families that are not fail-fast, and the
 *  iterator SHAPE each of them hands out.
 *
 *  The residual's own text says the check "has nowhere to run" because the
 *  iterator is the real `Arrays$ArrayItr` whose `next()` is real bytecode.
 *  That is a claim about four specific classes, and this prints them, in the
 *  mode that matters, instead of assuming one answer covers all four. Where
 *  the class already carries an `expectedModCount` the existing third
 *  fail-fast door can be reached; where it is `Arrays$ArrayItr` it cannot.
 */
public class FailFastShapeProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + tag + " |" + v + "|");
    }

    static void shape(String tag, Iterable<?> c) {
        p(tag + " itr", c.iterator().getClass().getName());
    }

    /** Does a structural change during iteration fail fast? */
    static void failFast(String tag, Iterable<?> c, Runnable mutate) {
        try {
            Iterator<?> it = c.iterator();
            it.next();
            mutate.run();
            it.next();
            p(tag + " failfast", "no-throw");
        } catch (ConcurrentModificationException e) {
            p(tag + " failfast", "CME");
        } catch (Throwable e) {
            p(tag + " failfast", "THREW " + e.getClass().getName());
        }
    }

    public static void main(String[] a) {
        TreeSet<String> ts = new TreeSet<>(List.of("a", "b", "c"));
        shape("TreeSet", ts);
        failFast("TreeSet add", new TreeSet<>(List.of("a", "b", "c")),
                 () -> { });

        ArrayDeque<String> ad = new ArrayDeque<>(List.of("a", "b", "c"));
        shape("ArrayDeque", ad);

        PriorityQueue<String> pq = new PriorityQueue<>(List.of("a", "b", "c"));
        shape("PriorityQueue", pq);

        TreeMap<String, String> tm = new TreeMap<>();
        tm.put("a", "1");
        tm.put("b", "2");
        tm.put("c", "3");
        shape("TreeMap keySet", tm.keySet());
        shape("TreeMap values", tm.values());
        shape("TreeMap entrySet", tm.entrySet());

        // The two that ARE fail-fast today, as the control.
        shape("ArrayList", new ArrayList<>(List.of("a", "b", "c")));
        shape("HashMap values", new HashMap<>(Map.of("k", "v")).values());

        // Does each family maintain a modCount a check could read?
        p("TreeSet size", ts.size());
        p("ArrayDeque size", ad.size());
        p("PriorityQueue size", pq.size());

        System.out.println("DONE FailFastShapeProbe");
    }
}
