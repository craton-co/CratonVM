import java.util.*;

/**
 * Is the map modification generation maintained per family?
 *
 * `bump_map_mod_count` is the invalidation generation any cached/lazy map view
 * would key on, and it is called from the HashMap-family put/remove/clear and a
 * TreeMap setter — but NOT from `native_lhm_put_evict` / `native_lhm_remove`.
 * A missing bump is observable from Java as a fail-fast iterator that does not
 * fail: structurally modifying a map while iterating it must throw
 * ConcurrentModificationException.
 *
 * Prints one row per family: CME = correct, NONE = the generation is not being
 * maintained for that family (or its iterator does not consult it).
 */
public class MapModCountProbe {

    static String probe(String label, Map<String, String> m, boolean removeInstead) {
        for (int i = 0; i < 8; i++) { m.put("k" + i, "v" + i); }
        try {
            for (Map.Entry<String, String> e : m.entrySet()) {
                if (removeInstead) { m.remove("k7"); } else { m.put("added", "x"); }
            }
            return "NONE";
        }
        catch (ConcurrentModificationException e) { return "CME"; }
        catch (Throwable t) { return t.getClass().getSimpleName(); }
    }

    /**
     * `values()` is the THIRD fail-fast door — an ArrayList-shaped carrier
     * reaching `native_al_iterator`, not the HashSet-shaped one the keySet and
     * entrySet columns exercise. It went unmeasured while that door was open,
     * so the table read as twelve cells when it is sixteen.
     */
    static String probeValues(String label, Map<String, String> m) {
        for (int i = 0; i < 8; i++) { m.put("k" + i, "v" + i); }
        try {
            for (String v : m.values()) { m.put("added", "x"); }
            return "NONE";
        }
        catch (ConcurrentModificationException e) { return "CME"; }
        catch (Throwable t) { return t.getClass().getSimpleName(); }
    }

    static String probeKeys(String label, Map<String, String> m) {
        for (int i = 0; i < 8; i++) { m.put("k" + i, "v" + i); }
        try {
            for (String k : m.keySet()) { m.put("added", "x"); }
            return "NONE";
        }
        catch (ConcurrentModificationException e) { return "CME"; }
        catch (Throwable t) { return t.getClass().getSimpleName(); }
    }

    /** One family, one map instance per cell — a probe that threw must not
     *  leave a half-mutated map behind for the next column. */
    static void row(String name, Map<String, String> a, Map<String, String> b,
                    Map<String, String> c, Map<String, String> d) {
        System.out.printf(
                "MODCOUNT %-22s entrySet.put=%-6s entrySet.remove=%-6s keySet.put=%-6s values.put=%s%n",
                name, probe(name, a, false), probe(name, b, true),
                probeKeys(name, c), probeValues(name, d));
    }

    public static void main(String[] args) {
        row("HashMap", new HashMap<>(), new HashMap<>(), new HashMap<>(), new HashMap<>());
        row("LinkedHashMap", new LinkedHashMap<>(), new LinkedHashMap<>(),
                new LinkedHashMap<>(), new LinkedHashMap<>());
        row("TreeMap", new TreeMap<>(), new TreeMap<>(), new TreeMap<>(), new TreeMap<>());
        row("Hashtable", new Hashtable<>(), new Hashtable<>(), new Hashtable<>(),
                new Hashtable<>());

        // A live view must see a put made through the map after the view was taken.
        Map<String, String> m = new LinkedHashMap<>();
        m.put("a", "1");
        Set<String> ks = m.keySet();
        Collection<String> vs = m.values();
        Set<Map.Entry<String, String>> es = m.entrySet();
        m.put("b", "2");
        System.out.printf("LIVEVIEW LinkedHashMap keySet=%d values=%d entrySet=%d (expect 2 2 2)%n",
                ks.size(), vs.size(), es.size());

        Map<String, String> h = new HashMap<>();
        h.put("a", "1");
        Set<String> hks = h.keySet();
        h.put("b", "2");
        System.out.printf("LIVEVIEW HashMap       keySet=%d (expect 2)%n", hks.size());
    }
}
