import java.util.LinkedHashMap;
import java.util.HashMap;
import java.util.Map;

/**
 * Is `keySet()` itself O(n) per call?
 *
 *   viewOnly   map.keySet() and nothing else          — pure view construction
 *   hoisted    keySet() ONCE, iterate inside the loop — the same reads, no rebuild
 *   perCall    keySet() inside the loop, then iterate — what Spring does
 *   sizeOnly   keySet().size() inside the loop
 *   valuesOnly map.values() and nothing else        — the values construction term
 *   entryOnly  map.entrySet() and nothing else      — the entrySet construction term
 *
 * If viewOnly ~= perCall and hoisted is fast, the cost is building the view, not
 * reading through it.
 */
public class KeySetBench {

    static int sink;
    static Object osink;

    public static void main(String[] args) {
        String rung = args.length > 0 ? args[0] : "viewOnly";
        int outer = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;
        int width = args.length > 2 ? Integer.parseInt(args[2]) : 1000;
        boolean hash = args.length > 3 && args[3].equals("hashmap");

        Map<String, Object> map = hash ? new HashMap<>() : new LinkedHashMap<>();
        for (int i = 0; i < width; i++) {
            String name = "test-7-property-" + i;
            map.put(name, name + "-value");
        }
        Iterable<String> hoistedKeys = map.keySet();

        long t0 = System.nanoTime();
        for (int k = 0; k < outer; k++) {
            switch (rung) {
                case "viewOnly" -> { osink = map.keySet(); }
                case "hoisted"  -> { for (String s : hoistedKeys) { if (s != null) { sink++; } } }
                case "perCall"  -> { for (String s : map.keySet()) { if (s != null) { sink++; } } }
                case "sizeOnly" -> { sink += map.keySet().size(); }
                case "valuesOnly" -> { osink = map.values(); }
                case "entryOnly" -> { osink = map.entrySet(); }
                case "mapSize"  -> { sink += map.size(); }
                default -> throw new IllegalArgumentException(rung);
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        double usPerCall = (ms * 1000.0) / outer;
        System.out.printf("KEYSET rung=%-9s map=%s outer=%d width=%d ms=%d us/call=%.1f sink=%d%n",
                rung, hash ? "HashMap" : "LinkedHashMap", outer, width, ms, usPerCall, sink);
    }
}
