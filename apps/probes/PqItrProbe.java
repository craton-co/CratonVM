import java.util.*;

/** The smallest program that shows whether a PriorityQueue iterator sees its
 *  elements. Two constructions, because the one that broke was built from a
 *  Collection and the ones that passed were not. */
public class PqItrProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + tag + " |" + v + "|");
    }

    static void walk(String tag, PriorityQueue<Integer> q) {
        p(tag + " size", q.size());
        Iterator<Integer> it = q.iterator();
        p(tag + " itr class", it.getClass().getName());
        p(tag + " hasNext", it.hasNext());
        StringBuilder sb = new StringBuilder();
        int guard = 0;
        while (it.hasNext() && guard++ < 10) sb.append(it.next()).append(',');
        p(tag + " walked", sb.toString());
    }

    public static void main(String[] a) {
        walk("fromCollection", new PriorityQueue<>(Arrays.asList(1, 2, 3)));

        PriorityQueue<Integer> byAdd = new PriorityQueue<>();
        byAdd.add(1);
        byAdd.add(2);
        byAdd.add(3);
        walk("byAdd", byAdd);

        // And the same question for the other two families this change touched.
        ArrayDeque<Integer> d = new ArrayDeque<>(Arrays.asList(1, 2, 3));
        p("deque size", d.size());
        StringBuilder ds = new StringBuilder();
        for (Integer x : d) ds.append(x).append(',');
        p("deque walked", ds.toString());

        TreeSet<Integer> t = new TreeSet<>(Arrays.asList(1, 2, 3));
        StringBuilder ts = new StringBuilder();
        for (Integer x : t) ts.append(x).append(',');
        p("treeset walked", ts.toString());

        System.out.println("DONE PqItrProbe");
    }
}
