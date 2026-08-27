import java.util.*;

/**
 * Behavioural parity for `java.util.ArrayList$Itr` once `hasNext`/`next` yield
 * to the real JDK cursor while `remove()` stays a native.
 *
 * That split is the whole risk. `Itr` keeps three pieces of state --
 * `cursor`, `lastRet`, `expectedModCount` -- and after this change two of its
 * three methods read/write them as REAL BYTECODE while the third writes them
 * from Rust. Every row below exists to catch the two halves disagreeing:
 * fail-fast (`expectedModCount` vs the list's `modCount`), the `lastRet`
 * contract (`remove()` before `next()`, twice in a row), and the cursor
 * rewind `remove()` owes the next `next()`.
 *
 * Also covers the OTHER collections that hand out an `ArrayList`-family
 * cursor, since they share the machinery: sublists, `Arrays.asList`,
 * `Collections.unmodifiable*`, `CopyOnWriteArrayList` (snapshot semantics --
 * must NOT throw), and the map views.
 */
public class ItrYieldProbe {
    static void p(String k, Object v) { System.out.println("ITR " + k + "=" + v); }
    static void ex(String k, Runnable r) {
        try { r.run(); p(k, "no-throw"); }
        catch (Throwable t) { p(k, t.getClass().getName()); }
    }
    static List<String> mk(int n) {
        List<String> l = new ArrayList<>();
        for (int i = 0; i < n; i++) l.add("e" + i);
        return l;
    }

    public static void main(String[] args) {
        // ---- basic walk ----
        StringBuilder sb = new StringBuilder();
        for (String s : mk(5)) sb.append(s).append(',');
        p("walk", sb);
        p("emptyHasNext", mk(0).iterator().hasNext());
        ex("emptyNext", () -> mk(0).iterator().next());

        // ---- exhaustion ----
        Iterator<String> it = mk(2).iterator();
        p("n1", it.next()); p("n2", it.next()); p("hasNextEnd", it.hasNext());
        ex("pastEnd", it::next);

        // ---- fail-fast: structural change mid-iteration ----
        ex("cmeAdd", () -> { List<String> l = mk(3); for (String s : l) l.add("x"); });
        ex("cmeRemove", () -> { List<String> l = mk(3); for (String s : l) l.remove(0); });
        ex("cmeClear", () -> { List<String> l = mk(3); for (String s : l) l.clear(); });
        // set() is NOT structural — must NOT throw
        ex("setNoCme", () -> { List<String> l = mk(3); for (String s : l) l.set(0, "z"); });

        // ---- Iterator.remove(): the native/bytecode split ----
        List<String> r1 = mk(4);
        Iterator<String> ri = r1.iterator();
        ri.next(); ri.remove();
        p("rmFirst", r1);
        p("rmContinues", ri.hasNext());
        p("rmNextAfter", ri.next());
        // remove() twice without next() in between
        List<String> r2 = mk(3);
        Iterator<String> ri2 = r2.iterator();
        ri2.next(); ri2.remove();
        ex("rmTwice", ri2::remove);
        // remove() before any next()
        ex("rmBeforeNext", () -> mk(3).iterator().remove());
        // remove every element
        List<String> r3 = mk(4);
        for (Iterator<String> i3 = r3.iterator(); i3.hasNext(); ) { i3.next(); i3.remove(); }
        p("rmAll", r3 + "|size=" + r3.size());
        // remove alternating, then keep walking
        List<String> r4 = mk(6);
        StringBuilder seen = new StringBuilder();
        int k = 0;
        for (Iterator<String> i4 = r4.iterator(); i4.hasNext(); ) {
            String v = i4.next(); seen.append(v).append(',');
            if (k++ % 2 == 0) i4.remove();
        }
        p("rmAltSeen", seen);
        p("rmAltLeft", r4);
        // after iterator remove, a fresh iterator must be fine
        p("rmThenWalk", String.join("-", r4));

        // ---- ListIterator ----
        List<String> li = mk(3);
        ListIterator<String> l2 = li.listIterator();
        l2.next(); l2.set("Z"); l2.add("Y");
        p("listItr", li);
        p("listItrPrev", l2.hasPrevious() ? l2.previous() : "none");

        // ---- other ArrayList-family cursors ----
        p("subList", String.join(",", mk(5).subList(1, 4)));
        ex("subListCme", () -> { List<String> l = mk(5); List<String> s = l.subList(1,4); l.add("q"); for (String x : s) {} });
        p("arraysAsList", String.join(",", Arrays.asList("a","b","c")));
        ex("arraysAsListRm", () -> { Iterator<String> i = Arrays.asList("a","b").iterator(); i.next(); i.remove(); });
        p("unmodifiable", String.join(",", Collections.unmodifiableList(mk(3))));
        ex("unmodifiableRm", () -> { Iterator<String> i = Collections.unmodifiableList(mk(3)).iterator(); i.next(); i.remove(); });

        // ---- COW: snapshot, must NOT throw ----
        List<String> cow = new java.util.concurrent.CopyOnWriteArrayList<>(mk(3));
        StringBuilder cb = new StringBuilder();
        for (String s : cow) { cb.append(s); cow.add("late"); }
        p("cowSnapshot", cb + "|size=" + cow.size());
        ex("cowItrRm", () -> { Iterator<String> i = new java.util.concurrent.CopyOnWriteArrayList<>(mk(2)).iterator(); i.next(); i.remove(); });

        // ---- map views share the machinery ----
        Map<String,String> m = new LinkedHashMap<>();
        for (int i = 0; i < 4; i++) m.put("k"+i, "v"+i);
        p("keys", String.join(",", m.keySet()));
        p("vals", String.join(",", m.values()));
        StringBuilder es = new StringBuilder();
        for (Map.Entry<String,String> e : m.entrySet()) es.append(e.getKey()).append('=').append(e.getValue()).append(',');
        p("entries", es);
        ex("keysCme", () -> { Map<String,String> mm = new LinkedHashMap<>(m); for (String s : mm.keySet()) mm.put("new","x"); });
        Map<String,String> m2 = new LinkedHashMap<>(m);
        for (Iterator<String> i5 = m2.keySet().iterator(); i5.hasNext(); ) { if (i5.next().equals("k1")) i5.remove(); }
        p("keysRmWriteThrough", m2.toString());

        // ---- copy constructors / toArray go through the same cursors ----
        p("copyList", new ArrayList<>(mk(3)));
        p("copySet", new LinkedHashSet<>(mk(3)).size());
        p("toArray", Arrays.toString(mk(3).toArray()));
        p("stream", mk(4).stream().filter(s -> !s.equals("e1")).count());
    }
}
