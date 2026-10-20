// The iterator shapes that reach `alloc_arraylist_iterator_as`'s
// `AL_VIEW_ITR_CLASS` mint -- a backing with no real `modCount`.
//
// `--jdk-only` refuses to fabricate that class, so this probe is the one that
// says what the refusal RECOVERY costs. Read the two modes against each other,
// not only against HotSpot: a row that differs between them is the price of the
// snapshot fallback and belongs in the record.
//
// Hygiene: stdout only, no identity hashes, and every collection is drained
// into a SORTED list before printing -- a values() view has no specified
// iteration order and both VMs may choose freely.
import java.util.ArrayList;
import java.util.Collection;
import java.util.Collections;
import java.util.ConcurrentModificationException;
import java.util.Hashtable;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Properties;
import java.util.TreeMap;

public class L3ViewItrSweep {
    interface Body { Object run() throws Throwable; }

    static void t(String tag, Body b) {
        String v;
        try {
            Object o = b.run();
            v = String.valueOf(o);
        } catch (Throwable e) {
            v = "throws " + e.getClass().getName();
        }
        System.out.println(tag + " = " + v);
    }

    static List<String> sorted(Iterable<?> src) {
        List<String> out = new ArrayList<>();
        for (Object o : src) out.add(String.valueOf(o));
        Collections.sort(out);
        return out;
    }

    static Properties props(int n) {
        Properties p = new Properties();
        for (int i = 0; i < n; i++) p.setProperty("k" + i, "v" + i);
        return p;
    }

    public static void main(String[] a) {
        plainIteration();
        removeThrough();
        removeContract();
        concurrentModification();
        System.out.println("DONE");
    }

    // ---- the shape RJdkEnumerations died on: read-only iteration ----

    static void plainIteration() {
        t("props.values.sorted", () -> sorted(props(5).values()));
        t("props.keySet.sorted", () -> sorted(props(5).keySet()));
        t("props.entrySet.size", () -> props(5).entrySet().size());
        t("hashtable.values.sorted", () -> {
            Hashtable<String, String> h = new Hashtable<>();
            h.put("a", "1");
            h.put("b", "2");
            return sorted(h.values());
        });
        t("hashmap.values.sorted", () -> {
            Map<String, String> m = new LinkedHashMap<>();
            m.put("a", "1");
            m.put("b", "2");
            return sorted(m.values());
        });
        t("treemap.values.sorted", () -> {
            Map<String, String> m = new TreeMap<>();
            m.put("a", "1");
            m.put("b", "2");
            return sorted(m.values());
        });
        // The exact route in the stack trace: a synchronized wrapper over a
        // values view, iterated with a for-each.
        t("syncCollection.values.sorted", () -> {
            Collection<Object> c = Collections.synchronizedCollection(props(4).values());
            return sorted(c);
        });
        t("syncList.sorted", () -> {
            List<String> l = Collections.synchronizedList(new ArrayList<>(List.of("x", "y")));
            return sorted(l);
        });
        t("props.values.emptyIteration", () -> sorted(new Properties().values()));
        t("props.values.hasNextOnEmpty", () -> new Properties().values().iterator().hasNext());
    }

    // ---- remove() through the view iterator must reach the MAP ----

    static void removeThrough() {
        t("props.values.itrRemove", () -> {
            Properties p = props(4);
            Iterator<Object> it = p.values().iterator();
            it.next();
            it.remove();
            return p.size();
        });
        t("props.values.itrRemoveAll", () -> {
            Properties p = props(4);
            Iterator<Object> it = p.values().iterator();
            while (it.hasNext()) {
                it.next();
                it.remove();
            }
            return p.size() + "/" + sorted(p.values());
        });
        t("hashtable.values.itrRemove", () -> {
            Hashtable<String, String> h = new Hashtable<>();
            h.put("a", "1");
            h.put("b", "2");
            Iterator<String> it = h.values().iterator();
            it.next();
            it.remove();
            return h.size();
        });
        t("hashmap.values.itrRemoveOne", () -> {
            Map<String, String> m = new LinkedHashMap<>();
            m.put("a", "1");
            m.put("b", "2");
            m.put("c", "3");
            Iterator<String> it = m.values().iterator();
            it.next();
            it.remove();
            return m.size();
        });
        // The removal must delete the ENTRY, not merely the value -- so the
        // key that carried it is gone from the map too.
        t("props.values.itrRemoveDropsKey", () -> {
            Properties p = new Properties();
            p.setProperty("only", "value");
            Iterator<Object> it = p.values().iterator();
            it.next();
            it.remove();
            return p.getProperty("only") + "/" + p.size();
        });
    }

    // ---- the IllegalStateException half of the Iterator contract ----

    static void removeContract() {
        t("props.values.removeBeforeNext", () -> {
            Iterator<Object> it = props(3).values().iterator();
            it.remove();
            return "no-throw";
        });
        t("props.values.removeTwice", () -> {
            Properties p = props(3);
            Iterator<Object> it = p.values().iterator();
            it.next();
            it.remove();
            it.remove();
            return "no-throw";
        });
        t("props.values.nextPastEnd", () -> {
            Iterator<Object> it = new Properties().values().iterator();
            it.next();
            return "no-throw";
        });
    }

    // ---- where a snapshot fallback CANNOT match a live view ----
    //
    // A live values() iterator throws ConcurrentModificationException when the
    // map is structurally modified underneath it; an iterator over a snapshot
    // has nothing to detect. Measured rather than assumed, and in BOTH modes,
    // because the answer is allowed to differ between them here and that
    // difference is the price of the refusal recovery.

    static void concurrentModification() {
        t("props.values.cmeOnPut", () -> {
            Properties p = props(4);
            Iterator<Object> it = p.values().iterator();
            it.next();
            p.setProperty("late", "arrival");
            it.next();
            return "no-throw";
        });
        t("props.values.cmeOnRemove", () -> {
            Properties p = props(4);
            Iterator<Object> it = p.values().iterator();
            it.next();
            p.remove("k0");
            it.next();
            return "no-throw";
        });
        t("hashmap.values.cmeOnPut", () -> {
            Map<String, String> m = new LinkedHashMap<>();
            m.put("a", "1");
            m.put("b", "2");
            Iterator<String> it = m.values().iterator();
            it.next();
            m.put("c", "3");
            it.next();
            return "no-throw";
        });
    }
}
