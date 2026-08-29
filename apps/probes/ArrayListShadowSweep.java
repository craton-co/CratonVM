import java.util.*;

/** L3 tail / `java.util.ArrayList` (19 rows), `ArrayList$SubList` (23),
 *  `ArrayList$ListItr` (6), `ArrayList$Itr` (3) and `Arrays$ArrayList` (4) —
 *  55 owning bridge-with-code registrations, the largest unclaimed block in
 *  `java.util` after the four named families.
 *
 *  `ArrayList$SubList` alone carries more registrations than `ArrayList`, and a
 *  sub-list is the hardest thing in the class to reproduce: it is a live view
 *  with an OFFSET and its own `modCount` mirror, so three separate contracts
 *  have to hold at once —
 *
 *    * its indices are relative to the view, and every bound is checked against
 *      the VIEW's size rather than the backing list's;
 *    * a structural change made THROUGH the view resizes the parent, and one
 *      made to the PARENT invalidates the view with
 *      `ConcurrentModificationException` on its next use — not silently wrong
 *      answers, which is what an offset-plus-length shim without a modCount
 *      mirror produces;
 *    * `subList(a,b).clear()` is the documented idiom for a range delete, so a
 *      view whose `clear` only empties itself leaves the parent untouched.
 *
 *  `Arrays.asList` is the fixed-size adapter: `set` writes THROUGH to the
 *  backing array and `add`/`remove` are `UnsupportedOperationException`. A
 *  shim that copies into an `ArrayList` gets every one of those three wrong
 *  while looking right on `get`.
 *
 *  DETERMINISM: list order is fully specified everywhere here.
 */
public class ArrayListShadowSweep {
    static int rows = 0;
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    interface ThrowingRun { void run() throws Throwable; }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface Call { Object call() throws Throwable; }
    static void tv(String tag, Call r) {
        try { p(tag, "ok " + String.valueOf(r.call())); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    static ArrayList<String> l4() {
        return new ArrayList<>(Arrays.asList("a", "b", "c", "d"));
    }

    // ------------------------------------------------------------------
    // 1. bounds, and the two indices that are legal for add but not for get
    // ------------------------------------------------------------------
    static void bounds() {
        ArrayList<String> l = l4();
        p("get 0", l.get(0));
        p("get last", l.get(3));
        t("get -1", () -> l.get(-1));
        t("get size", () -> l.get(4));
        t("get MIN_VALUE", () -> l.get(Integer.MIN_VALUE));
        t("set -1", () -> l.set(-1, "x"));
        t("set size", () -> l.set(4, "x"));
        p("set returns old", l.set(1, "B"));
        t("add at size", () -> l.add(4, "e"));
        p("after add at size", l.toString());
        t("add at size+1", () -> l.add(6, "f"));
        t("add at -1", () -> l.add(-1, "f"));
        t("remove(int) size", () -> l.remove(l.size()));
        t("remove(int) -1", () -> l.remove(-1));
        p("remove(int) returns old", l.remove(0));
        p("after remove(int)", l.toString());
        // remove(Object) and remove(int) are different methods; an Integer
        // element makes the overload choice visible.
        ArrayList<Object> ov = new ArrayList<>(Arrays.asList(Integer.valueOf(10),
                                                             Integer.valueOf(20)));
        p("remove(int) picks by index", ov.remove(0));
        p("after remove(int) on Integer list", ov.toString());
        p("remove(Object) picks by value", ov.remove(Integer.valueOf(20)));
        p("after remove(Object)", ov.toString());

        t("addAll at size", () -> l.addAll(l.size(), Arrays.asList("x")));
        t("addAll at size+1", () -> l.addAll(l.size() + 1, Arrays.asList("x")));
        t("addAll(int) null", () -> l.addAll(0, null));
        t("addAll null", () -> l.addAll(null));
        p("addAll empty returns false", l.addAll(new ArrayList<>()));
        p("state", l.toString());

        t("new ArrayList(-1)", () -> new ArrayList<String>(-1));
        t("new ArrayList(0)", () -> new ArrayList<String>(0));
        tv("new ArrayList(0) usable", () -> { ArrayList<String> z = new ArrayList<>(0);
                                              z.add("a"); return z.toString(); });
        t("new ArrayList(null)", () -> new ArrayList<String>((Collection<String>) null));
        t("ensureCapacity negative", () -> l.ensureCapacity(-1));
        t("trimToSize", () -> l.trimToSize());
        p("after trimToSize", l.toString());

        // nulls are ordinary elements in an ArrayList
        ArrayList<String> n = new ArrayList<>();
        p("add null", n.add(null));
        n.add("a"); n.add(null);
        p("toString with nulls", n.toString());
        p("contains null", n.contains(null));
        p("indexOf null", n.indexOf(null));
        p("lastIndexOf null", n.lastIndexOf(null));
        p("indexOf absent", n.indexOf("zz"));
        p("lastIndexOf absent", n.lastIndexOf("zz"));
        p("remove(Object) null", n.remove(null));
        p("after remove null", n.toString());
    }

    // ------------------------------------------------------------------
    // 2. iterator, listIterator and the fail-fast contract
    // ------------------------------------------------------------------
    static void iterators() {
        ArrayList<String> l = l4();
        Iterator<String> it = l.iterator();
        t("iterator remove before next", () -> it.remove());
        p("iterator next", it.next());
        it.remove();
        p("after iterator remove", l.toString());
        t("iterator remove twice", () -> it.remove());
        while (it.hasNext()) it.next();
        t("iterator next past end", () -> it.next());

        ArrayList<String> ff = l4();
        t("fail fast on add during iteration", () -> { for (String x : ff) ff.add("z"); });
        ArrayList<String> ff2 = l4();
        t("fail fast on remove during iteration", () -> { for (String x : ff2) ff2.remove("a"); });
        ArrayList<String> ff3 = l4();
        t("fail fast on clear during forEach", () -> ff3.forEach(x -> ff3.clear()));
        // a SET during iteration is NOT structural and must not throw
        ArrayList<String> nf = l4();
        t("set during iteration is not structural", () -> {
            for (int i = 0; i < nf.size(); i++) { nf.set(i, nf.get(i) + "!"); }
        });
        p("after non-structural set", nf.toString());

        ListIterator<String> li = l4().listIterator();
        p("listIterator nextIndex at start", li.nextIndex());
        p("listIterator previousIndex at start", li.previousIndex());
        p("listIterator hasPrevious at start", li.hasPrevious());
        t("listIterator previous at start", () -> li.previous());
        t("listIterator set before next", () -> li.set("x"));
        li.next();
        li.set("A");
        li.add("A2");
        p("listIterator nextIndex after add", li.nextIndex());
        t("listIterator set after add", () -> li.set("y"));
        ArrayList<String> lil = l4();
        ListIterator<String> li2 = lil.listIterator(lil.size());
        p("listIterator(size) hasNext", li2.hasNext());
        p("listIterator(size) previous", li2.previous());
        t("listIterator(-1)", () -> lil.listIterator(-1));
        t("listIterator(size+1)", () -> lil.listIterator(lil.size() + 1));
        ArrayList<String> lir = l4();
        ListIterator<String> li3 = lir.listIterator();
        li3.next(); li3.previous(); li3.remove();
        p("listIterator remove after previous", lir.toString());
    }

    // ------------------------------------------------------------------
    // 3. subList — the offset view, its bounds and its modCount mirror
    // ------------------------------------------------------------------
    static void subList() {
        ArrayList<String> l = l4();
        p("subList content", l.subList(1, 3).toString());
        p("subList size", l.subList(1, 3).size());
        p("subList empty", l.subList(2, 2).toString());
        p("subList whole", l.subList(0, l.size()).toString());
        t("subList from > to", () -> l.subList(3, 1));
        t("subList negative from", () -> l.subList(-1, 2));
        t("subList to past end", () -> l.subList(0, 9));

        // indices are RELATIVE to the view
        List<String> v = l.subList(1, 3);
        p("view get 0 is parent 1", v.get(0));
        t("view get at view size", () -> v.get(2));
        t("view get -1", () -> v.get(-1));
        p("view indexOf", v.indexOf("c"));
        p("view indexOf outside range", v.indexOf("a"));
        p("view contains outside range", v.contains("d"));

        // writes go through, both ways
        v.set(0, "B");
        p("view set wrote through", l.toString());
        l.set(1, "b2");
        p("parent set visible in view", v.toString());
        v.add("NEW");
        p("view add resized parent", l.toString());
        p("view size after add", v.size());
        v.remove("NEW");
        p("view remove wrote through", l.toString());

        // a sub-view of a sub-view
        List<String> vv = l.subList(1, 4).subList(1, 2);
        p("sub of sub", vv.toString());
        vv.set(0, "Z");
        p("sub of sub wrote through to root", l.toString());

        // subList(a,b).clear() is the documented range-delete idiom
        ArrayList<String> cl = l4();
        cl.subList(1, 3).clear();
        p("subList clear removed the range", cl.toString());
        p("subList clear parent size", cl.size());

        // a structural change to the PARENT invalidates the view
        ArrayList<String> par = l4();
        List<String> stale = par.subList(1, 3);
        par.add("e");
        t("stale view get", () -> stale.get(0));
        t("stale view size", () -> stale.size());
        t("stale view iterator", () -> stale.iterator().next());
        t("stale view set", () -> stale.set(0, "x"));
        // ... but a non-structural parent write does not
        ArrayList<String> par2 = l4();
        List<String> live = par2.subList(1, 3);
        par2.set(0, "A");
        tv("view after non-structural parent write", () -> live.toString());

        p("view equals a plain list", l4().subList(1, 3).equals(Arrays.asList("b", "c")));
        p("view hashCode agrees",
            l4().subList(1, 3).hashCode() == Arrays.asList("b", "c").hashCode());
        p("view toArray", Arrays.toString(l4().subList(1, 3).toArray()));
        p("view sort", sortView());
        p("view removeIf", removeIfView());
        p("view listIterator", l4().subList(1, 3).listIterator().next());
        p("view addAll", addAllView());
    }
    static String sortView() {
        ArrayList<String> l = new ArrayList<>(Arrays.asList("d", "c", "b", "a"));
        l.subList(1, 3).sort(Comparator.naturalOrder());
        return l.toString();
    }
    static String removeIfView() {
        ArrayList<String> l = l4();
        List<String> v = l.subList(1, 3);
        boolean r = v.removeIf(x -> x.equals("b"));
        return r + " " + l.toString() + " " + v.toString();
    }
    static String addAllView() {
        ArrayList<String> l = l4();
        List<String> v = l.subList(1, 3);
        v.addAll(Arrays.asList("X", "Y"));
        return l.toString() + " " + v.toString();
    }

    // ------------------------------------------------------------------
    // 4. Arrays.asList — the fixed-size, write-through adapter
    // ------------------------------------------------------------------
    static void arraysAsList() {
        String[] backing = { "a", "b", "c" };
        List<String> al = Arrays.asList(backing);
        p("asList content", al.toString());
        p("asList size", al.size());
        p("asList get", al.get(1));
        t("asList get out of range", () -> al.get(3));
        p("asList set returns old", al.set(1, "B"));
        // the write went to the ARRAY, not to a copy
        p("asList set wrote through to the array", Arrays.toString(backing));
        t("asList add", () -> al.add("d"));
        t("asList add(int)", () -> al.add(0, "d"));
        t("asList remove(Object)", () -> al.remove("a"));
        t("asList remove(int)", () -> al.remove(0));
        t("asList clear", () -> al.clear());
        t("asList addAll", () -> al.addAll(Arrays.asList("d")));
        t("asList removeIf that matches nothing", () -> al.removeIf(x -> false));
        t("asList removeIf that matches", () -> al.removeIf(x -> x.equals("a")));
        t("asList iterator remove", () -> { Iterator<String> i = al.iterator(); i.next(); i.remove(); });
        t("asList sort", () -> al.sort(Comparator.reverseOrder()));
        p("asList after sort", al.toString());
        p("asList after sort wrote through", Arrays.toString(backing));
        t("asList replaceAll", () -> al.replaceAll(x -> x + "!"));
        p("asList after replaceAll", Arrays.toString(backing));
        p("asList indexOf", al.indexOf("a!"));
        p("asList contains", al.contains("b!"));
        p("asList equals an ArrayList", Arrays.asList("a", "b")
            .equals(new ArrayList<>(Arrays.asList("a", "b"))));
        p("asList hashCode agrees", Arrays.asList("a", "b").hashCode()
            == new ArrayList<>(Arrays.asList("a", "b")).hashCode());
        p("asList subList", Arrays.asList("a", "b", "c").subList(1, 3).toString());
        t("asList subList set", () -> Arrays.asList("a", "b", "c").subList(1, 3).set(0, "x"));
        t("asList null array", () -> Arrays.asList((Object[]) null));
        p("asList empty", Arrays.asList().toString());
        p("asList with a null element", Arrays.asList("a", null).toString());
        p("asList toArray", Arrays.toString(Arrays.asList("a", "b").toArray()));
        // the JDK's Arrays$ArrayList.toArray() returns a COPY: writing to it
        // must not disturb the backing array.
        String[] b2 = { "a", "b" };
        Object[] copy = Arrays.asList(b2).toArray();
        copy[0] = "Z";
        p("asList toArray is a copy", Arrays.toString(b2));
        // a single non-array argument is a one-element list, not a splat
        p("asList of one array-typed argument", Arrays.asList(new int[] { 1, 2 }).size());
    }

    // ------------------------------------------------------------------
    // 5. the rest of the List surface
    // ------------------------------------------------------------------
    static void listSurface() {
        ArrayList<String> l = l4();
        p("toArray", Arrays.toString(l.toArray()));
        p("toArray typed exact", Arrays.toString(l.toArray(new String[4])));
        p("toArray typed short", Arrays.toString(l.toArray(new String[0])));
        String[] big = new String[6];
        Arrays.fill(big, "Z");
        l.toArray(big);
        p("toArray typed long nulls the slot after", Arrays.toString(big));
        t("toArray null", () -> l.toArray((String[]) null));
        t("toArray wrong element type", () -> l.toArray(new Integer[4]));

        p("removeIf", l.removeIf(x -> x.equals("b")));
        p("after removeIf", l.toString());
        p("removeIf no match", l.removeIf(x -> false));
        t("removeIf null", () -> l.removeIf(null));
        l.replaceAll(x -> x + "!");
        p("replaceAll", l.toString());
        t("replaceAll null", () -> l.replaceAll(null));
        p("retainAll", l.retainAll(Arrays.asList("a!", "c!")));
        p("after retainAll", l.toString());
        p("removeAll", l.removeAll(Arrays.asList("a!")));
        p("after removeAll", l.toString());
        t("retainAll null", () -> l.retainAll(null));
        t("removeAll null", () -> l.removeAll(null));
        p("containsAll", l.containsAll(Arrays.asList("c!")));
        p("containsAll empty", l.containsAll(new ArrayList<>()));
        t("containsAll null", () -> l.containsAll(null));

        ArrayList<String> s = new ArrayList<>(Arrays.asList("c", "a", "b"));
        s.sort(Comparator.naturalOrder());
        p("sort", s.toString());
        s.sort(null);
        p("sort(null) is natural order", s.toString());
        ArrayList<Object> ns = new ArrayList<>(Arrays.asList("a", null));
        t("sort with a null element and natural order", () -> ns.sort(null));

        p("clone", ((ArrayList<String>) l4().clone()).toString());
        p("clone is shallow but independent", cloneIndependent());
        p("equals a LinkedList", l4().equals(new LinkedList<>(Arrays.asList("a","b","c","d"))));
        p("equals a Set (must be false)", l4().equals(new HashSet<>(Arrays.asList("a"))));
        p("hashCode agrees with LinkedList", l4().hashCode()
            == new LinkedList<>(Arrays.asList("a","b","c","d")).hashCode());
        p("empty list hashCode", new ArrayList<String>().hashCode());
        p("empty toString", new ArrayList<String>().toString());
        ArrayList<String> c = l4();
        c.clear();
        p("clear", c.toString());
        p("clear size", c.size());
        p("clear isEmpty", c.isEmpty());
    }
    static String cloneIndependent() {
        ArrayList<String> a = l4();
        @SuppressWarnings("unchecked")
        ArrayList<String> b = (ArrayList<String>) a.clone();
        b.add("e");
        return a.size() + "/" + b.size();
    }

    public static void main(String[] args) {
        bounds();
        iterators();
        subList();
        arraysAsList();
        listSurface();
        System.out.println("ROWS " + rows);
        System.out.println("DONE ArrayListShadowSweep");
    }
}
