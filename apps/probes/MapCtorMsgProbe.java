import java.util.*;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Does the JDK use ONE spelling for the map-constructor argument checks?
 *
 * `map_ctor_capacity_load_check` in native-collections is a shared helper over
 * four callers, and its own comment argues for keeping one spelling: "this file
 * has twice found a rule half-applied across a family that shares the contract
 * and not the code." That is the right instinct when the family DOES share the
 * contract. This asks whether it does.
 *
 * Every row is a message, printed verbatim, so a difference is visible rather
 * than inferred. Rows are ordered by class so the families line up.
 */
public class MapCtorMsgProbe {

    static int rows = 0;

    static void row(String what, Runnable r) {
        String cls = "no-throw", msg = "no-throw";
        try {
            r.run();
        } catch (Throwable t) {
            cls = t.getClass().getName();
            msg = String.valueOf(t.getMessage());
        }
        System.out.println((++rows) + " " + what + " |" + cls + "| msg |" + msg + "|");
    }

    public static void main(String[] args) {
        // Negative initial capacity.
        row("HashMap(-1)", () -> new HashMap<>(-1));
        row("LinkedHashMap(-1)", () -> new LinkedHashMap<>(-1));
        row("HashSet(-1)", () -> new HashSet<>(-1));
        row("LinkedHashSet(-1)", () -> new LinkedHashSet<>(-1));
        row("Hashtable(-1)", () -> new Hashtable<>(-1));
        row("ConcurrentHashMap(-1)", () -> new ConcurrentHashMap<>(-1));
        row("WeakHashMap(-1)", () -> new WeakHashMap<>(-1));
        row("IdentityHashMap(-1)", () -> new IdentityHashMap<>(-1));

        // Non-positive / NaN load factor.
        row("HashMap(16, 0f)", () -> new HashMap<>(16, 0f));
        row("HashMap(16, NaN)", () -> new HashMap<>(16, Float.NaN));
        row("LinkedHashMap(16, 0f)", () -> new LinkedHashMap<>(16, 0f));
        row("HashSet(16, 0f)", () -> new HashSet<>(16, 0f));
        row("Hashtable(16, 0f)", () -> new Hashtable<>(16, 0f));
        row("ConcurrentHashMap(16, 0f, 1)", () -> new ConcurrentHashMap<>(16, 0f, 1));
        row("WeakHashMap(16, 0f)", () -> new WeakHashMap<>(16, 0f));

        // Null source map.
        row("HashMap(null)", () -> new HashMap<>((Map<String, String>) null));
        row("LinkedHashMap(null)", () -> new LinkedHashMap<>((Map<String, String>) null));
        row("Hashtable(null)", () -> new Hashtable<>((Map<String, String>) null));
        row("ConcurrentHashMap(null)", () -> new ConcurrentHashMap<>((Map<String, String>) null));
        row("TreeMap(null map)", () -> new TreeMap<>((Map<String, String>) null));

        // putAll(null), the same check reached through a method.
        row("HashMap.putAll(null)", () -> new HashMap<String, String>().putAll(null));
        row("Hashtable.putAll(null)", () -> new Hashtable<String, String>().putAll(null));
        row("ConcurrentHashMap.putAll(null)",
            () -> new ConcurrentHashMap<String, String>().putAll(null));

        // Exhausted enumerations/iterators: does the JDK give a message?
        row("Hashtable.elements past end", () -> {
            Enumeration<String> e = new Hashtable<String, String>().elements();
            e.nextElement();
        });
        row("ConcurrentHashMap.elements past end", () -> {
            Enumeration<String> e = new ConcurrentHashMap<String, String>().elements();
            e.nextElement();
        });
        row("Vector.elements past end", () -> {
            Enumeration<String> e = new Vector<String>().elements();
            e.nextElement();
        });
        row("ArrayList.iterator past end", () -> new ArrayList<String>().iterator().next());
        row("HashMap.keySet iterator past end",
            () -> new HashMap<String, String>().keySet().iterator().next());

        System.out.println("ROWS " + rows);
        System.out.println("DONE MapCtorMsgProbe");
    }
}
