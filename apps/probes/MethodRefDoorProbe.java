import java.util.*;

/** Minimal repro for the one Properties row still differing: shrinking the
 *  program, not the flag (`the-first-invocation-difference-that-reproduces-is-
 *  not-the-condition`). Two shapes that differ only in HOW the entries were
 *  written -- `setProperty` versus `Map.put` -- because that is the only
 *  difference between the failing `PropertiesShadowSweep` row and the passing
 *  `MapViewsShadowSweep` one.
 */
public class MethodRefDoorProbe {
    static int rows = 0;
    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + tag + " |" + String.valueOf(v) + "|");
    }
    static void t(String tag, Runnable r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    public static void main(String[] args) {
        Properties a = new Properties();
        a.setProperty("x", "1");
        a.setProperty("y", "2");
        Set<Object> ks = a.keySet();
        p("setProperty keySet", ks.size());
        Iterator<Object> it = ks.iterator();
        p("setProperty next", it.next() != null);
        t("setProperty remove", it::remove);
        p("setProperty size after", a.size());

        Properties b = new Properties();
        b.put("x", "1");
        b.put("y", "2");
        Iterator<Object> it2 = b.keySet().iterator();
        p("put next", it2.next() != null);
        t("put remove", it2::remove);
        p("put size after", b.size());

        // Three entries, the MapViews shape exactly.
        Properties c = new Properties();
        c.put("a", "1"); c.put("b", "2"); c.put("c", "3");
        Iterator<Object> it3 = c.keySet().iterator();
        it3.next();
        t("three-entry remove", it3::remove);
        p("three-entry size after", c.size());

        // The keySet taken ONCE and iterated twice.
        Properties d = new Properties();
        d.setProperty("x", "1");
        d.setProperty("y", "2");
        Set<Object> dks = d.keySet();
        Iterator<Object> i1 = dks.iterator();
        i1.next();
        t("first iterator remove", i1::remove);
        Iterator<Object> i2 = dks.iterator();
        i2.next();
        t("second iterator remove", i2::remove);
        p("size after two removes", d.size());
        // The SAME operation with a DIRECT call instead of a method reference.
        // `it::remove` is an invokedynamic-built bound MethodHandle, a
        // different dispatch door; if only that one fails the defect is in the
        // handle machinery, not in the collection.
        Properties e = new Properties();
        e.setProperty("x", "1");
        e.setProperty("y", "2");
        Iterator<Object> ie = e.keySet().iterator();
        ie.next();
        try { ie.remove(); p("direct remove", "no-throw"); }
        catch (Throwable ex) { p("direct remove", "THREW " + ex.getClass().getName()); }
        p("direct remove size after", e.size());

        Properties f = new Properties();
        f.put("a", "1"); f.put("b", "2"); f.put("c", "3");
        Iterator<Object> iff = f.keySet().iterator();
        iff.next();
        try { iff.remove(); p("direct three-entry remove", "no-throw"); }
        catch (Throwable ex) { p("direct three-entry remove", "THREW " + ex.getClass().getName()); }
        p("direct three-entry size after", f.size());

        // Is the method-reference door wrong for EVERY receiver, or only for a
        // Properties keySet? Four more containers, the same two doors each.
        List<String> al = new ArrayList<>(Arrays.asList("a", "b", "c"));
        Iterator<String> ali = al.iterator();
        ali.next();
        t("ArrayList itr::remove", ali::remove);
        p("ArrayList size after", al.size());

        Set<String> hs = new HashSet<>(Arrays.asList("a", "b", "c"));
        Iterator<String> hsi = hs.iterator();
        hsi.next();
        t("HashSet itr::remove", hsi::remove);
        p("HashSet size after", hs.size());

        Map<String, String> hm = new HashMap<>();
        hm.put("a", "1"); hm.put("b", "2");
        Iterator<String> hmi = hm.keySet().iterator();
        hmi.next();
        t("HashMap keySet itr::remove", hmi::remove);
        p("HashMap size after", hm.size());

        Hashtable<String, String> ht = new Hashtable<>();
        ht.put("a", "1"); ht.put("b", "2");
        Iterator<String> hti = ht.keySet().iterator();
        hti.next();
        t("Hashtable keySet itr::remove", hti::remove);
        p("Hashtable size after", ht.size());

        // ... and a non-iterator receiver, to separate "method reference" from
        // "Iterator.remove".
        List<String> ml = new ArrayList<>(Arrays.asList("a", "b"));
        Runnable clear = ml::clear;
        clear.run();
        p("ArrayList ml::clear size after", ml.size());

        System.out.println("ROWS " + rows);
        System.out.println("DONE MethodRefDoorProbe");
    }
}
