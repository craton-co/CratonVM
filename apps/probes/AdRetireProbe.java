import java.util.*;

/** L3 §6.1 — can `ArrayDeque`'s iterator be RETIRED to real JDK bytecode?
 *
 *  The fail-fast row this probe exists for is not a `modCount` row. The JDK's
 *  `DeqIterator` has no `modCount`: it holds a PHYSICAL index into the ring
 *  buffer and `nonNullElementAt` turns any null it reads into a CME. That is
 *  why HotSpot is asymmetric — an `add` that grows the buffer slides the data
 *  and leaves a null under the cursor, while a `remove` from the head slides
 *  the cursor's own side and never does. No generation counter reproduces that;
 *  only the physical layout does.
 *
 *  We already maintain that layout, so the honest fix is to stop shadowing the
 *  iterator at all. `descendingIterator()` is the control: it ALREADY runs real
 *  bytecode over our array, so whatever it answers here is what a retired
 *  `iterator()` would answer.
 *
 *  The rows are ordered control-first. If the descending rows diverge the
 *  layout is wrong and retirement is off the table; if they match, the only
 *  question left is whether real `delete()` leaves our slot-3 `size` stale.
 */
public class AdRetireProbe {
    static int rows = 0;

    interface ThrowingRun { void run() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\n").replace("\r", "\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    static ArrayDeque<String> abc() {
        return new ArrayDeque<>(Arrays.asList("a", "b", "c"));
    }

    public static void main(String[] args) {
        // ---- CONTROL: the descending iterator, which is real bytecode today.
        p("desc class", abc().descendingIterator().getClass().getName());
        p("desc walk", drain(abc().descendingIterator()));

        ArrayDeque<String> d1 = abc();
        t("desc fail fast on add", () -> {
            Iterator<String> i = d1.descendingIterator();
            while (i.hasNext()) { i.next(); d1.add("z"); }
        });
        ArrayDeque<String> d2 = abc();
        t("desc fail fast on remove", () -> {
            Iterator<String> i = d2.descendingIterator();
            while (i.hasNext()) { i.next(); d2.remove("a"); }
        });

        // Does real `delete()` keep every OTHER observable honest? If our
        // size lives in a field real bytecode never writes, this is where it
        // shows: size() and isEmpty() come from our natives, toString() and
        // the walk from the array.
        ArrayDeque<String> d3 = abc();
        Iterator<String> i3 = d3.descendingIterator();
        i3.next();
        i3.remove();
        p("desc remove -> toString", d3.toString());
        p("desc remove -> size", d3.size());
        p("desc remove -> isEmpty", d3.isEmpty());
        p("desc remove -> peekFirst", d3.peekFirst());
        p("desc remove -> peekLast", d3.peekLast());
        p("desc remove -> contains c", d3.contains("c"));
        p("desc remove -> toArray", Arrays.toString(d3.toArray()));
        p("desc remove -> walk again", drain(d3.iterator()));
        p("desc remove -> add then toString", addAnd(d3, "n"));
        p("desc remove -> stream count", d3.stream().count());

        // Two deletions from the middle, which take `delete`'s other branch.
        ArrayDeque<String> d4 = new ArrayDeque<>(Arrays.asList("a", "b", "c", "d", "e"));
        Iterator<String> i4 = d4.iterator();
        i4.next(); i4.next();
        i4.remove();
        p("mid remove -> toString", d4.toString());
        p("mid remove -> size", d4.size());
        p("mid remove -> stream count", d4.stream().count());

        // ---- The rows §6.1 is about, for reference in the same run.
        ArrayDeque<String> f1 = abc();
        t("itr fail fast on add", () -> { for (String x : f1) f1.add("z"); });
        ArrayDeque<String> f2 = abc();
        t("itr fail fast on remove", () -> { for (String x : f2) f2.remove("a"); });
        p("itr class", abc().iterator().getClass().getName());

        // A deque that has WRAPPED, where head > tail. Retirement has to be
        // right here too, and it is the state a snapshot iterator cannot see.
        ArrayDeque<String> w = new ArrayDeque<>(4);
        w.add("a"); w.add("b"); w.add("c");
        w.removeFirst(); w.removeFirst();
        w.add("d"); w.add("e");
        p("wrapped toString", w.toString());
        p("wrapped size", w.size());
        p("wrapped walk", drain(w.iterator()));
        p("wrapped desc walk", drain(w.descendingIterator()));
        t("wrapped fail fast on add", () -> {
            for (String x : w) w.add("z");
        });

        // Growth without an iterator open, to separate "grow is wrong" from
        // "grow is wrong DURING iteration".
        ArrayDeque<String> g = new ArrayDeque<>(2);
        for (int k = 0; k < 9; k++) g.add("e" + k);
        p("grown toString", g.toString());
        p("grown size", g.size());
        p("grown stream count", g.stream().count());

        System.out.println("DONE AdRetireProbe");
    }

    static String addAnd(ArrayDeque<String> d, String s) {
        d.add(s);
        return d.toString() + " size=" + d.size();
    }

    static String drain(Iterator<String> i) {
        StringBuilder sb = new StringBuilder();
        while (i.hasNext()) sb.append(i.next()).append(',');
        return sb.toString();
    }
}
