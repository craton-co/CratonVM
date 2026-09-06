import java.util.*;

/** `Namespace.getTables()` is `tables.values()` on a live map, and
 *  `collectTableMappings()` feeds it to `ArrayList.addAll`, which reads
 *  `toArray()`. The map grows between the two calls. */
public class StaleViewAddAllProbe {
    static int fails = 0;
    static void chk(String what, int got, int want) {
        boolean ok = got == want;
        if (!ok) fails++;
        System.out.println((ok ? "OK   " : "FAIL ") + what + " = " + got + " (want " + want + ")");
    }

    static void round(String tag, Map<String, String> m) {
        m.put("a", "1");
        m.put("b", "2");
        m.put("c", "3");
        // first read, exactly as collectTableMappings does
        List<String> first = new ArrayList<>();
        first.addAll(m.values());
        chk(tag + " first addAll", first.size(), 3);
        chk(tag + " first toArray", m.values().toArray().length, 3);
        // the map grows, as Envers' contribution grows the namespace
        m.put("d", "4");
        m.put("e", "5");
        List<String> second = new ArrayList<>();
        second.addAll(m.values());
        chk(tag + " after growth addAll", second.size(), 5);
        chk(tag + " after growth toArray", m.values().toArray().length, 5);
        chk(tag + " after growth size()", m.values().size(), 5);
        int viaIterator = 0;
        for (String s : m.values()) viaIterator++;
        chk(tag + " after growth for-each", viaIterator, 5);
        chk(tag + " after growth new ArrayList<>(values)", new ArrayList<>(m.values()).size(), 5);
        // hold the view across the growth, the shape Namespace.getTables() has
        // when a caller keeps the returned Collection
        Collection<String> held = m.values();
        m.put("f", "6");
        chk(tag + " held view size after growth", held.size(), 6);
        chk(tag + " held view toArray after growth", held.toArray().length, 6);
        List<String> third = new ArrayList<>();
        third.addAll(held);
        chk(tag + " held view addAll after growth", third.size(), 6);
        // keySet and entrySet, same question
        chk(tag + " keySet toArray", m.keySet().toArray().length, 6);
        chk(tag + " entrySet toArray", m.entrySet().toArray().length, 6);
    }

    public static void main(String[] a) {
        round("TreeMap", new TreeMap<>());
        round("HashMap", new HashMap<>());
        round("LinkedHashMap", new LinkedHashMap<>());
        System.out.println(fails == 0 ? "PROBE-OK" : "PROBE-FAIL " + fails);
    }
}
