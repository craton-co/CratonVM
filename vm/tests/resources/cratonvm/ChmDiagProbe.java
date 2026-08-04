// JAVA21+
package cratonvm;

import java.util.HashMap;
import java.util.concurrent.ConcurrentHashMap;

/**
 * Diagnostic companion to {@link ChmBasicProbe}. Everything it learns is
 * printed, so a single in-process invocation reports the whole ladder at once
 * instead of one bit per run.
 */
public class ChmDiagProbe {

    public static int diag() {
        try {
            String k0 = "k" + 0;
            String k0b = "k" + 0;
            System.out.println("k0=[" + k0 + "] len=" + k0.length()
                    + " hash=" + k0.hashCode() + " hash2=" + k0b.hashCode()
                    + " equals=" + k0.equals(k0b) + " same=" + (k0 == k0b));

            HashMap<String, Integer> hm = new HashMap<>();
            for (int i = 0; i < 11; i++) {
                hm.put("k" + i, i);
            }
            System.out.println("hashmap size=" + hm.size() + " get(k0)=" + hm.get("k0")
                    + " get(k5)=" + hm.get("k5"));

            ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
            for (int i = 0; i < 11; i++) {
                m.put("k" + i, i);
            }
            System.out.println("chm size=" + m.size());
            for (int i = 0; i < 11; i++) {
                System.out.println("  chm.get(k" + i + ") = " + m.get("k" + i)
                        + "  containsKey=" + m.containsKey("k" + i));
            }
            System.out.println("chm.get literal k0 = " + m.get("k0"));
            System.out.println("chm keys = " + m.keySet());
            return 7;
        } catch (Throwable t) {
            System.out.println("diag threw " + t);
            return -1;
        }
    }

    /** The exact body of ChmBasicProbe.testChmPreResizePutGet, with tracing. */
    public static int preResizeTraced() {
        ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
        for (int i = 0; i < 11; i++) {
            map.put("k" + i, i);
        }
        if (map.size() != 11) {
            System.out.println("stage: size mismatch " + map.size());
            return -1;
        }
        for (int i = 0; i < 11; i++) {
            Integer v = map.get("k" + i);
            if (v == null) {
                System.out.println("stage: null at " + i);
                return -100 - i;
            }
            if (v.intValue() != i) {
                System.out.println("stage: wrong value at " + i + " -> " + v);
                return -200 - i;
            }
        }
        System.out.println("stage: all good, returning 1");
        return 1;
    }

    /** Smallest possible: does a plain int-returning method round-trip at all? */
    public static int returnsOne() {
        return 1;
    }

    /** Does a method that declares `throws Throwable` round-trip its return? */
    public static int returnsOneThrows() throws Throwable {
        try {
            return 1;
        } catch (Throwable t) {
            throw t;
        }
    }
}
