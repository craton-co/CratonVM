import java.util.*;
import java.lang.reflect.*;

/** L3 §6.1 — the physical state of an `ArrayDeque`, read straight off the fields.
 *
 *  `AdRetireProbe` showed `descendingIterator().remove()` leaving a null inside
 *  the deque. Every explanation for that is a claim about `elements`, `head`
 *  and `tail`, so this stops arguing and prints them. Reflection reads the real
 *  fields whichever mode is running, which makes the three arms directly
 *  comparable.
 */
public class AdFieldProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + tag + " |" + String.valueOf(v) + "|");
    }

    static String state(String tag, ArrayDeque<?> d) throws Exception {
        Field fe = ArrayDeque.class.getDeclaredField("elements");
        Field fh = ArrayDeque.class.getDeclaredField("head");
        Field ft = ArrayDeque.class.getDeclaredField("tail");
        fe.setAccessible(true); fh.setAccessible(true); ft.setAccessible(true);
        Object[] es = (Object[]) fe.get(d);
        return "cap=" + (es == null ? -1 : es.length)
             + " head=" + fh.getInt(d) + " tail=" + ft.getInt(d)
             + " es=" + Arrays.toString(es);
    }

    static String itr(Object it) throws Exception {
        Class<?> c = it.getClass();
        while (c != null && c.getDeclaredFields().length == 0) c = c.getSuperclass();
        // DeqIterator declares cursor/remaining/lastRet.
        Class<?> d = it.getClass();
        while (d != null) {
            try {
                Field a = d.getDeclaredField("cursor");
                Field b = d.getDeclaredField("remaining");
                Field e = d.getDeclaredField("lastRet");
                a.setAccessible(true); b.setAccessible(true); e.setAccessible(true);
                return "cursor=" + a.getInt(it) + " remaining=" + b.getInt(it)
                     + " lastRet=" + e.getInt(it);
            } catch (NoSuchFieldException nf) { d = d.getSuperclass(); }
        }
        return "no-fields";
    }

    public static void main(String[] args) throws Exception {
        ArrayDeque<String> d = new ArrayDeque<>(Arrays.asList("a", "b", "c"));
        p("fresh", state("fresh", d));

        Iterator<String> di = d.descendingIterator();
        p("desc itr class", di.getClass().getName());
        p("desc itr fresh", itr(di));
        p("desc next", di.next());
        p("desc itr after next", itr(di));
        di.remove();
        p("desc after remove", state("after", d));
        p("desc itr after remove", itr(di));
        p("desc size()", d.size());

        ArrayDeque<String> e = new ArrayDeque<>(Arrays.asList("a", "b", "c"));
        Iterator<String> ei = e.iterator();
        p("fwd itr class", ei.getClass().getName());
        p("fwd itr fresh", itr(ei));
        p("fwd next", ei.next());
        p("fwd itr after next", itr(ei));
        ei.remove();
        p("fwd after remove", state("after", e));
        p("fwd size()", e.size());

        ArrayDeque<String> g = new ArrayDeque<>(Arrays.asList("a", "b", "c"));
        g.add("z");
        p("after add 4th", state("g", g));
        g.add("y");
        p("after add 5th", state("g", g));

        ArrayDeque<String> n = new ArrayDeque<>();
        p("default empty", state("n", n));
        ArrayDeque<String> n2 = new ArrayDeque<>(4);
        p("ctor(4) empty", state("n2", n2));

        System.out.println("DONE AdFieldProbe");
    }
}
