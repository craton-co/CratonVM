import org.testng.collections.Maps;

import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

// SPR-AOT-TESTNG-MAPS.1 repro: org/testng/collections/Maps' tiny allocation
// factories (newConcurrentMap/newHashMap) are suspected of handing back a
// malformed/empty map that later makes computeIfAbsent appear to return null.
// Stress the exact factory + computeIfAbsent path many times to cross JIT
// invocation thresholds.
public class TestNgMapsProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int failures = 0;

        for (int i = 0; i < iterations; i++) {
            Map<String, Integer> concurrent = Maps.newConcurrentMap();
            Map<String, Integer> plain = Maps.newHashMap();

            final int fi = i;
            String key = "k" + i;
            Integer v1 = concurrent.computeIfAbsent(key, k -> fi * 3 + 1);
            Integer v2 = plain.computeIfAbsent(key, k -> fi * 5 + 2);

            if (v1 == null || v1 != i * 3 + 1) {
                failures++;
                if (failures <= 5) {
                    System.out.println("CONCURRENT MISMATCH at i=" + i + " got=" + v1);
                }
            }
            if (v2 == null || v2 != i * 5 + 2) {
                failures++;
                if (failures <= 5) {
                    System.out.println("PLAIN MISMATCH at i=" + i + " got=" + v2);
                }
            }

            // Second computeIfAbsent call on the same key must NOT recompute.
            Integer v1b = concurrent.computeIfAbsent(key, k -> -999);
            if (!v1b.equals(v1)) {
                failures++;
                if (failures <= 5) {
                    System.out.println("RECOMPUTE MISMATCH at i=" + i + " first=" + v1 + " second=" + v1b);
                }
            }

            if (!(concurrent instanceof ConcurrentHashMap)) {
                failures++;
                if (failures <= 2) {
                    System.out.println("newConcurrentMap() did not return a ConcurrentHashMap at i=" + i
                            + " actual=" + concurrent.getClass());
                }
            }

            if (i % 2000 == 0) {
                System.out.println("progress i=" + i);
                System.out.flush();
            }
        }
        System.out.println("DONE iterations=" + iterations + " failures=" + failures);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
