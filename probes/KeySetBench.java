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
 *   toArrHoisted  keySet().toArray(new String[0]) on a hoisted view
 *   toArrPerCall  keySet().toArray(new String[0]) per call — the true Spring shape
 *   toArrPlain    keySet().toArray() on a hoisted view — the untyped door
 *   iterList/iterSet/idxList  controls: the same 1000 strings in a plain
 *                 ArrayList / HashSet, so "the iterator protocol is slow" can
 *                 be told apart from "the map VIEW's iterator is slow"
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
        java.util.Set<String> hoistedSet = map.keySet();
        // Controls that separate "the iterator protocol is slow" from "the
        // VIEW's iterator is slow": the same 1000 strings in an ArrayList and
        // in a plain HashSet, neither of which is a live map view.
        java.util.List<String> plainList = new java.util.ArrayList<>(hoistedSet);
        java.util.Set<String> plainSet = new java.util.HashSet<>(hoistedSet);
        // The FLOOR: no collection at all. If this is also ~0.8 us/element the
        // cost is the loop itself, and no collection fix can touch it.
        String[] rawArr = hoistedSet.toArray(new String[0]);

        long t0 = System.nanoTime();
        for (int k = 0; k < outer; k++) {
            switch (rung) {
                case "viewOnly" -> { osink = map.keySet(); }
                case "hoisted"  -> { for (String s : hoistedKeys) { if (s != null) { sink++; } } }
                case "perCall"  -> { for (String s : map.keySet()) { if (s != null) { sink++; } } }
                case "sizeOnly" -> { sink += map.keySet().size(); }
                case "valuesOnly" -> { osink = map.values(); }
                case "entryOnly" -> { osink = map.entrySet(); }
                // `toArray` is a DIFFERENT door from the iterator, and it is the
                // one Spring actually takes: `getPropertyNames()` is
                // `StringUtils.toStringArray(map.keySet())`, i.e.
                // `Collection.toArray(new String[0])`. Measuring the iterator
                // and calling it "what Spring does" conflates two paths whose
                // per-element costs are not the same number.
                case "toArrHoisted" -> { osink = hoistedSet.toArray(new String[0]); }
                case "toArrPerCall" -> { osink = map.keySet().toArray(new String[0]); }
                case "toArrPlain"   -> { osink = hoistedSet.toArray(); }
                case "iterList" -> { for (String t : plainList) { if (t != null) { sink++; } } }
                case "iterSet"  -> { for (String t : plainSet)  { if (t != null) { sink++; } } }
                case "idxList"  -> { for (int j = 0; j < plainList.size(); j++) { if (plainList.get(j) != null) { sink++; } } }
                case "rawArr"   -> { for (int j = 0; j < rawArr.length; j++) { if (rawArr[j] != null) { sink++; } } }
                case "rawArrFe" -> { for (String t : rawArr) { if (t != null) { sink++; } } }
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
