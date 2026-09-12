/**
 * L1 wave 7's diagnosis instrument: what STATE does each map family actually
 * carry, and how much of it is real-JDK heap state the image's own bodies
 * could read?
 *
 * RUN IT WITH THE FLAG on both VMs, or every reflective row reads
 * `THREW java.lang.reflect.InaccessibleObjectException` on both and the probe
 * measures nothing:
 *
 *   java   --add-opens java.base/java.util=ALL-UNNAMED -cp out L1MapStateDiag
 *   cratonvm --java-home $JDK --jdk-only  *            --add-opens java.base/java.util=ALL-UNNAMED -cp out L1MapStateDiag
 *
 * Without it the two VMs agree on the refusal, which is why this file is safe
 * to leave in the shared battery -- it is 0-diff there and says nothing.
 *
 * What it said on 2026-09-12 (`cratonvm-l1w7-base-20260912`, UNARMED, against
 * HotSpot 25.0.4+7): 75 rows, ONE divergence (`LinkedHashMap.modCount` 5 vs
 * 3). `Hashtable.table` is a real `[Ljava.util.Hashtable$Entry;` with `count`
 * written, `LinkedHashMap`'s `head`/`tail`/`size`/`table` are the real fields
 * with real `LinkedHashMap$Entry` nodes, and `TreeMap.root` is a real
 * red-black tree -- so "state is a Rust side table" was false for all three
 * families the lane page held on that sentence.
 */
import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.util.*;

public class L1MapStateDiag {

    static void p(String k, Object v) { System.out.println(k + " = " + v); }

    static String cls(Object o) { return o == null ? "null" : o.getClass().getName(); }

    static void field(String tag, Object owner, Class<?> decl, String name) {
        try {
            Field f = decl.getDeclaredField(name);
            f.setAccessible(true);
            Object v = f.get(owner);
            if (v != null && v.getClass().isArray()) {
                int len = Array.getLength(v);
                int nonNull = 0;
                StringBuilder nodeCls = new StringBuilder();
                for (int i = 0; i < len; i++) {
                    Object e = Array.get(v, i);
                    if (e != null) {
                        nonNull++;
                        if (nodeCls.indexOf(cls(e)) < 0) {
                            if (nodeCls.length() > 0) nodeCls.append('+');
                            nodeCls.append(cls(e));
                        }
                    }
                }
                p(tag + "." + name, "arrayClass=" + cls(v) + " len=" + len
                        + " nonNull=" + nonNull + " nodeClasses=" + nodeCls);
            } else {
                p(tag + "." + name, "value=" + v + " class=" + cls(v));
            }
        } catch (Throwable t) {
            p(tag + "." + name, "THREW " + t.getClass().getName());
        }
    }

    static void views(String tag, Map<String, String> m) {
        p(tag + ".size", m.size());
        p(tag + ".isEmpty", m.isEmpty());
        try { p(tag + ".entrySet.size", m.entrySet().size()); }
        catch (Throwable t) { p(tag + ".entrySet.size", "THREW " + t.getClass().getName()); }
        try { p(tag + ".keySet.size", m.keySet().size()); }
        catch (Throwable t) { p(tag + ".keySet.size", "THREW " + t.getClass().getName()); }
        try { p(tag + ".values.size", m.values().size()); }
        catch (Throwable t) { p(tag + ".values.size", "THREW " + t.getClass().getName()); }
        try { p(tag + ".entrySet.toArray.len", m.entrySet().toArray().length); }
        catch (Throwable t) { p(tag + ".entrySet.toArray.len", "THREW " + t.getClass().getName()); }
        try { p(tag + ".keySet.toArray.len", m.keySet().toArray().length); }
        catch (Throwable t) { p(tag + ".keySet.toArray.len", "THREW " + t.getClass().getName()); }
        try { p(tag + ".entrySet.intoArrayList", new ArrayList<Map.Entry<String,String>>(m.entrySet()).size()); }
        catch (Throwable t) { p(tag + ".entrySet.intoArrayList", "THREW " + t.getClass().getName()); }
        try {
            int n = 0;
            for (Map.Entry<String, String> e : m.entrySet()) n++;
            p(tag + ".entrySet.forLoop", n);
        } catch (Throwable t) { p(tag + ".entrySet.forLoop", "THREW " + t.getClass().getName()); }
        try { p(tag + ".entrySet.viewClass", cls(m.entrySet())); }
        catch (Throwable t) { p(tag + ".entrySet.viewClass", "THREW " + t.getClass().getName()); }
        try { p(tag + ".entrySet.iterClass", cls(m.entrySet().iterator())); }
        catch (Throwable t) { p(tag + ".entrySet.iterClass", "THREW " + t.getClass().getName()); }
        try { p(tag + ".toString", m.toString()); }
        catch (Throwable t) { p(tag + ".toString", "THREW " + t.getClass().getName()); }
    }

    static <M extends Map<String, String>> M fill(M m) {
        m.put("a", "1"); m.put("b", "2"); m.put("c", "3");
        return m;
    }

    public static void main(String[] a) {
        Hashtable<String, String> ht = fill(new Hashtable<String, String>());
        p("A.hashtable.class", cls(ht));
        views("A.hashtable", ht);
        field("A.hashtable", ht, Hashtable.class, "table");
        field("A.hashtable", ht, Hashtable.class, "count");
        field("A.hashtable", ht, Hashtable.class, "modCount");
        field("A.hashtable", ht, Hashtable.class, "threshold");
        try { p("A.hashtable.keysEnum", count(ht.keys())); }
        catch (Throwable t) { p("A.hashtable.keysEnum", "THREW " + t.getClass().getName()); }
        try { p("A.hashtable.elemEnum", count(ht.elements())); }
        catch (Throwable t) { p("A.hashtable.elemEnum", "THREW " + t.getClass().getName()); }
        try { p("A.hashtable.clone.size", ((Hashtable<?, ?>) ht.clone()).size()); }
        catch (Throwable t) { p("A.hashtable.clone.size", "THREW " + t.getClass().getName()); }

        LinkedHashMap<String, String> lhm = fill(new LinkedHashMap<String, String>());
        p("B.lhm.class", cls(lhm));
        views("B.lhm", lhm);
        field("B.lhm", lhm, HashMap.class, "table");
        field("B.lhm", lhm, HashMap.class, "size");
        field("B.lhm", lhm, HashMap.class, "modCount");
        field("B.lhm", lhm, LinkedHashMap.class, "head");
        field("B.lhm", lhm, LinkedHashMap.class, "tail");
        field("B.lhm", lhm, LinkedHashMap.class, "accessOrder");

        HashMap<String, String> hm = fill(new HashMap<String, String>());
        p("C.hashmap.class", cls(hm));
        views("C.hashmap", hm);
        field("C.hashmap", hm, HashMap.class, "table");
        field("C.hashmap", hm, HashMap.class, "size");

        TreeMap<String, String> tm = fill(new TreeMap<String, String>());
        p("D.treemap.class", cls(tm));
        views("D.treemap", tm);
        field("D.treemap", tm, TreeMap.class, "root");
        field("D.treemap", tm, TreeMap.class, "size");
        field("D.treemap", tm, TreeMap.class, "comparator");

        Properties pr = new Properties();
        pr.setProperty("a", "1"); pr.setProperty("b", "2");
        p("E.props.class", cls(pr));
        p("E.props.size", pr.size());
        field("E.props", pr, Hashtable.class, "table");
        field("E.props", pr, Hashtable.class, "count");
        field("E.props", pr, Properties.class, "map");
    }

    static int count(Enumeration<?> e) {
        int n = 0;
        while (e.hasMoreElements()) { e.nextElement(); n++; }
        return n;
    }
}
