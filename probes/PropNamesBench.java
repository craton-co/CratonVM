import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Arrays;

/**
 * The OTHER half of every ConfigurationPropertySourcesTests cache access.
 *
 * `Cache.update` calls `propertySource.getPropertyNames()` and then
 * `Arrays.equals(lastUpdated, propertyNames)`. For a MapPropertySource backed by
 * a 1000-entry LinkedHashMap, `getPropertyNames()` is
 * `StringUtils.toStringArray(map.keySet())` — a fresh String[1000] built by
 * walking the keySet. Measured 101 910 of those per test run, i.e. ~100M
 * keySet steps, exactly matching the ~100M Arrays.equals element compares.
 *
 * Rungs:
 *   keyset     map.keySet().toArray(new String[0])
 *   forEach    for (String k : map.keySet()) — iteration only
 *   sizeOnly   map.keySet().size() — the cheapest possible touch
 *   arrayseq   Arrays.equals over the resulting arrays (for scale)
 */
public class PropNamesBench {

    static int sink;
    static Object osink;

    public static void main(String[] args) {
        String rung = args.length > 0 ? args[0] : "keyset";
        int outer = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;
        int width = args.length > 2 ? Integer.parseInt(args[2]) : 1000;

        Map<String, Object> map = new LinkedHashMap<>();
        for (int i = 0; i < width; i++) {
            String name = "test-7-property-" + i;
            map.put(name, name + "-value");
        }
        String[] prev = map.keySet().toArray(new String[0]);

        long t0 = System.nanoTime();
        for (int k = 0; k < outer; k++) {
            switch (rung) {
                case "keyset" -> { osink = map.keySet().toArray(new String[0]); }
                case "forEach" -> { for (String s : map.keySet()) { if (s != null) { sink++; } } }
                case "sizeOnly" -> { sink += map.keySet().size(); }
                case "arrayseq" -> { if (Arrays.equals(prev, prev.clone())) { sink++; } }
                case "both" -> {
                    String[] cur = map.keySet().toArray(new String[0]);
                    if (Arrays.equals(prev, cur)) { sink++; }
                }
                default -> throw new IllegalArgumentException(rung);
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        long elems = (long) outer * width;
        System.out.printf("PROPNAMES rung=%-9s elems=%d ms=%d ns/elem=%.1f sink=%d%n",
                rung, elems, ms, (ms * 1e6) / elems, sink);
    }
}
