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

    static String probeKeys(String label, Map<String, String> m) {
        for (int i = 0; i < 8; i++) { m.put("k" + i, "v" + i); }
        try {
            for (String k : m.keySet()) { m.put("added", "x"); }
            return "NONE";
        }
        catch (ConcurrentModificationException e) { return "CME"; }
        catch (Throwable t) { return t.getClass().getSimpleName(); }
    }

    public static void main(String[] args) {
        System.out.printf("MODCOUNT %-22s entrySet.put=%-6s entrySet.remove=%-6s keySet.put=%s%n",
                "HashMap", probe("hm", new HashMap<>(), false),
                probe("hm", new HashMap<>(), true), probeKeys("hm", new HashMap<>()));
        System.out.printf("MODCOUNT %-22s entrySet.put=%-6s entrySet.remove=%-6s keySet.put=%s%n",
                "LinkedHashMap", probe("lhm", new LinkedHashMap<>(), false),
                probe("lhm", new LinkedHashMap<>(), true), probeKeys("lhm", new LinkedHashMap<>()));
        System.out.printf("MODCOUNT %-22s entrySet.put=%-6s entrySet.remove=%-6s keySet.put=%s%n",
                "TreeMap", probe("tm", new TreeMap<>(), false),
                probe("tm", new TreeMap<>(), true), probeKeys("tm", new TreeMap<>()));
        System.out.printf("MODCOUNT %-22s entrySet.put=%-6s entrySet.remove=%-6s keySet.put=%s%n",
                "Hashtable", probe("ht", new Hashtable<>(), false),
                probe("ht", new Hashtable<>(), true), probeKeys("ht", new Hashtable<>()));

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
