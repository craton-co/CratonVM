import java.util.*;

/** L3 / `java.util.Hashtable` (24 rows) and the legacy `Vector`/`Stack` tail.
 *
 *  `Hashtable` is the classic shared-surface trap named in the lane brief:
 *  its natives sit in the same file as `HashMap`'s, and it refuses BOTH a null
 *  key and a null value where `HashMap` permits both. One body serving both
 *  receivers gets exactly one of them right.
 *
 *  The rest of the surface is legacy-specific and equally easy to get from
 *  memory rather than from the spec:
 *
 *    * `new Hashtable(0)` is LEGAL (capacity 0 is bumped to 1) while
 *      `new Hashtable(-1)` is `IllegalArgumentException`, and a `NaN` load
 *      factor has to be spelled out because every comparison against NaN is
 *      false;
 *    * `keys()`/`elements()` are `Enumeration`s, distinct objects from
 *      `keySet()`/`values()`, and an `Enumeration` has no `remove`;
 *    * `Vector`'s capacity/element API (`elementAt`, `setSize`,
 *      `insertElementAt`, `removeElementAt`) throws
 *      `ArrayIndexOutOfBoundsException` where the `List` API on the same object
 *      throws `IndexOutOfBoundsException` — the same index, two exception
 *      types, decided by which door you came in;
 *    * `Stack.pop`/`peek` on empty is `EmptyStackException`, not
 *      `NoSuchElementException`.
 *
 *  DETERMINISM: `Hashtable` iteration order is unspecified, so every
 *  enumeration and view is sorted before printing.
 */
public class HashtableVectorShadowSweep {
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
    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) l.add(String.valueOf(o));
        Collections.sort(l);
        return l.toString();
    }
    static String sortedEnum(Enumeration<?> e) {
        List<String> l = new ArrayList<>();
        while (e.hasMoreElements()) l.add(String.valueOf(e.nextElement()));
        Collections.sort(l);
        return l.toString();
    }

    // ------------------------------------------------------------------
    // 1. the null axis, both sides — the whole point of the family
    // ------------------------------------------------------------------
    static void nullAxis() {
        Hashtable<String, String> h = new Hashtable<>();
        h.put("a", "1");
        t("ht put null key", () -> h.put(null, "v"));
        t("ht put null value", () -> h.put("k", null));
        t("ht get null", () -> h.get(null));
        t("ht containsKey null", () -> h.containsKey(null));
        t("ht contains null", () -> h.contains(null));
        t("ht containsValue null", () -> h.containsValue(null));
        t("ht remove null", () -> h.remove(null));
        t("ht getOrDefault null key", () -> h.getOrDefault(null, "d"));
        t("ht putIfAbsent null key", () -> h.putIfAbsent(null, "v"));
        t("ht putIfAbsent null value", () -> h.putIfAbsent("k", null));
        t("ht putAll null", () -> h.putAll(null));
        t("ht putAll map with a null value", () -> {
            Map<String, String> m = new HashMap<>();
            m.put("z", null);
            h.putAll(m);
        });
        t("ht replace null value", () -> h.replace("a", null));
        t("ht merge null value", () -> h.merge("a", null, (x, y) -> x));
        t("ht computeIfAbsent null function", () -> h.computeIfAbsent("a", null));
        t("ht compute null function", () -> h.compute("a", null));
        t("ht forEach null", () -> h.forEach(null));
        p("ht unchanged", sorted(h.keySet()));
        p("ht size unchanged", h.size());

        // The contrast that names the trap: the same calls on a HashMap.
        HashMap<String, String> m = new HashMap<>();
        p("hm put null key", m.put(null, "v"));
        p("hm put null value", m.put("k", null));
        p("hm get null key", m.get(null));
        p("hm containsKey null", m.containsKey(null));
        p("hm containsValue null", m.containsValue(null));
        p("hm size", m.size());

        // a null RESULT from a functional update REMOVES on a Hashtable,
        // rather than storing null.
        Hashtable<String, String> f = new Hashtable<>();
        f.put("a", "1");
        p("ht computeIfAbsent null result", f.computeIfAbsent("b", k -> null));
        p("ht containsKey after null result", f.containsKey("b"));
        p("ht computeIfPresent null result removes", f.computeIfPresent("a", (k, v) -> null));
        p("ht size after removal by compute", f.size());
        f.put("c", "3");
        p("ht merge null result removes", f.merge("c", "x", (x, y) -> null));
        p("ht containsKey after merge null", f.containsKey("c"));
    }

    // ------------------------------------------------------------------
    // 2. Hashtable constructors and the rest of its Map surface
    // ------------------------------------------------------------------
    static void hashtable() {
        t("new Hashtable(-1)", () -> new Hashtable<String, String>(-1));
        t("new Hashtable(0)", () -> new Hashtable<String, String>(0));
        tv("new Hashtable(0) usable", () -> {
            Hashtable<String, String> z = new Hashtable<>(0);
            z.put("a", "1");
            return z.get("a");
        });
        t("new Hashtable(16, 0f)", () -> new Hashtable<String, String>(16, 0f));
        t("new Hashtable(16, -1f)", () -> new Hashtable<String, String>(16, -1f));
        t("new Hashtable(16, NaN)", () -> new Hashtable<String, String>(16, Float.NaN));
        t("new Hashtable(-1, 0.75f)", () -> new Hashtable<String, String>(-1, 0.75f));
        t("new Hashtable(Map) null", () -> new Hashtable<String, String>((Map<String, String>) null));
        t("new Hashtable(Map) with null value", () -> {
            Map<String, String> bad = new HashMap<>();
            bad.put("k", null);
            new Hashtable<>(bad);
        });
        p("new Hashtable(Map)", sorted(new Hashtable<>(
            Collections.singletonMap("a", "1")).keySet()));

        Hashtable<String, String> h = new Hashtable<>();
        p("put returns null", h.put("a", "1"));
        p("put returns old", h.put("a", "2"));
        p("get", h.get("a"));
        p("size", h.size());
        p("isEmpty", h.isEmpty());
        p("containsKey", h.containsKey("a"));
        p("containsKey absent", h.containsKey("zz"));
        p("contains value", h.contains("2"));
        p("containsValue", h.containsValue("2"));
        p("getOrDefault present", h.getOrDefault("a", "D"));
        p("getOrDefault absent", h.getOrDefault("zz", "D"));
        p("putIfAbsent present", h.putIfAbsent("a", "3"));
        p("putIfAbsent absent", h.putIfAbsent("b", "3"));
        p("remove returns old", h.remove("b"));
        p("remove absent", h.remove("b"));
        p("replace present", h.replace("a", "4"));
        p("replace absent", h.replace("zz", "4"));
        p("replace 3-arg wrong", h.replace("a", "WRONG", "5"));
        p("replace 3-arg right", h.replace("a", "4", "5"));
        p("state", h.get("a"));

        h.put("b", "6");
        p("keys", sortedEnum(h.keys()));
        p("elements", sortedEnum(h.elements()));
        p("keySet", sorted(h.keySet()));
        p("values", sorted(h.values()));
        p("entrySet", sorted(h.entrySet()));
        Enumeration<String> en = h.keys();
        en.nextElement();
        p("enumeration hasMoreElements", en.hasMoreElements());
        en.nextElement();
        p("enumeration exhausted", en.hasMoreElements());
        t("enumeration past end", en::nextElement);

        // views write through
        Hashtable<String, String> v = new Hashtable<>();
        v.put("x", "1"); v.put("y", "2");
        p("keySet.remove", v.keySet().remove("x"));
        p("keySet.remove wrote through", v.size());
        p("values.remove", v.values().remove("2"));
        p("values.remove wrote through", v.size());
        t("keySet.add unsupported", () -> v.keySet().add("q"));

        Hashtable<String, String> it = new Hashtable<>();
        it.put("x", "1"); it.put("y", "2");
        Iterator<String> i = it.keySet().iterator();
        i.next(); i.remove();
        p("keySet iterator remove", it.size());
        t("ht fail fast", () -> { for (String k : it.keySet()) it.put("z" + k, "9"); });

        // toString / equals / hashCode / clone
        Hashtable<String, String> one = new Hashtable<>();
        one.put("k", "v");
        p("toString one entry", one.toString());
        p("empty toString", new Hashtable<String, String>().toString());
        p("equals a HashMap", one.equals(Collections.singletonMap("k", "v")));
        p("hashCode agrees with HashMap",
            one.hashCode() == Collections.singletonMap("k", "v").hashCode());
        p("equals null", one.equals(null));
        p("clone", ((Hashtable<String, String>) one.clone()).get("k"));
        Hashtable<String, String> cl = (Hashtable<String, String>) one.clone();
        cl.put("k2", "v2");
        p("clone is independent", one.size());
        one.clear();
        p("clear", one.size());
        p("clear isEmpty", one.isEmpty());
    }

    // ------------------------------------------------------------------
    // 3. Vector and Stack — two exception types for one index
    // ------------------------------------------------------------------
    static void vectorStack() {
        Vector<String> v = new Vector<>(Arrays.asList("a", "b", "c"));
        p("v get", v.get(1));
        p("v elementAt", v.elementAt(1));
        p("v firstElement", v.firstElement());
        p("v lastElement", v.lastElement());
        // The List door and the legacy door disagree on the exception TYPE.
        t("v get out of range", () -> v.get(9));
        t("v elementAt out of range", () -> v.elementAt(9));
        t("v get negative", () -> v.get(-1));
        t("v elementAt negative", () -> v.elementAt(-1));
        t("v remove(int) out of range", () -> v.remove(9));
        t("v removeElementAt out of range", () -> v.removeElementAt(9));
        t("v add(int) out of range", () -> v.add(9, "x"));
        t("v insertElementAt out of range", () -> v.insertElementAt("x", 9));
        p("v insertElementAt at size is legal", insertAtSize());
        t("v set out of range", () -> v.set(9, "x"));
        t("v setElementAt out of range", () -> v.setElementAt("x", 9));
        t("v subList out of range", () -> v.subList(0, 9));

        Vector<String> e = new Vector<>();
        t("v firstElement on empty", e::firstElement);
        t("v lastElement on empty", e::lastElement);
        p("v empty isEmpty", e.isEmpty());
        p("v empty size", e.size());
        t("v new Vector(-1)", () -> new Vector<String>(-1));
        t("v new Vector(0)", () -> new Vector<String>(0));
        t("v new Vector(null)", () -> new Vector<String>((Collection<String>) null));
        p("v allows null elements", e.add(null));
        p("v contains null", e.contains(null));
        p("v indexOf null", e.indexOf(null));

        Vector<String> c = new Vector<>(Arrays.asList("a", "b", "c"));
        c.setSize(5);
        p("v setSize grows with nulls", c.toString());
        c.setSize(1);
        p("v setSize shrinks", c.toString());
        t("v setSize negative", () -> c.setSize(-1));
        p("v capacity is at least size", c.capacity() >= c.size());
        Vector<String> el = new Vector<>(Arrays.asList("a", "b"));
        p("v elements", sortedEnum(el.elements()));
        p("v removeElement", el.removeElement("a"));
        p("v removeElement absent", el.removeElement("zz"));
        p("v after removeElement", el.toString());
        el.addElement("c");
        p("v addElement", el.toString());
        el.removeAllElements();
        p("v removeAllElements", el.toString());
        p("v equals an ArrayList", new Vector<>(Arrays.asList("a"))
            .equals(new ArrayList<>(Arrays.asList("a"))));
        p("v copyInto", copyInto());
        t("v copyInto too small", () -> new Vector<>(Arrays.asList("a", "b"))
            .copyInto(new Object[1]));
        t("v copyInto null", () -> new Vector<>(Arrays.asList("a")).copyInto(null));

        Stack<String> s = new Stack<>();
        t("stack empty pop", s::pop);
        t("stack empty peek", s::peek);
        p("stack empty empty()", s.empty());
        p("stack search absent", s.search("a"));
        s.push("a"); s.push("b");
        p("stack push order", s.toString());
        p("stack peek", s.peek());
        p("stack search top is 1", s.search("b"));
        p("stack search deeper", s.search("a"));
        p("stack pop", s.pop());
        p("stack after pop", s.toString());
        p("stack is a List", s.get(0));
    }

    static String insertAtSize() {
        Vector<String> v = new Vector<>(Arrays.asList("a"));
        try { v.insertElementAt("b", 1); return v.toString(); }
        catch (Throwable x) { return "THREW " + x.getClass().getName(); }
    }
    static String copyInto() {
        Object[] a = new Object[3];
        new Vector<>(Arrays.asList("a", "b")).copyInto(a);
        return Arrays.toString(a);
    }

    public static void main(String[] args) {
        nullAxis();
        hashtable();
        vectorStack();
        System.out.println("ROWS " + rows);
        System.out.println("DONE HashtableVectorShadowSweep");
    }
}
