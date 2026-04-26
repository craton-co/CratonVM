// JAVA21+
package rustjvm;

import java.util.concurrent.ConcurrentHashMap;

/**
 * WP4.6 — ConcurrentHashMap regression probe (rust-jvm side).
 *
 * Verifies the K1-family fix at vm/src/runtime/value_stack.rs:385-460
 * (Value::Object(None)/Uninitialized → 0 in pop_int/pop_long) does not
 * regress under CHM's primitive-field-driven CAS retries.
 *
 * Each test method returns 1 on success, 0 on failure.  Wrapped in
 * try/catch so a thrown exception is also reported as 0.
 */
public class ChmBasicProbe {

    /** 1000 puts then 1000 gets; size must equal 1000. */
    public static int testChmBasicPutGet() {
        try {
            ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
            for (int i = 0; i < 1000; i++) {
                map.put("k" + i, i);
            }
            if (map.size() != 1000) return 0;
            for (int i = 0; i < 1000; i++) {
                Integer v = map.get("k" + i);
                if (v == null || v.intValue() != i) return 0;
            }
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * Pre-resize baseline: 11 entries stays under the 0.75 × 16 = 12-entry
     * load threshold so {@code transfer()} is never invoked. Pins the
     * working subset of CHM put/get on rust-jvm even when the larger
     * post-resize path regresses.
     */
    public static int testChmPreResizePutGet() {
        try {
            ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
            for (int i = 0; i < 11; i++) {
                map.put("k" + i, i);
            }
            if (map.size() != 11) return 0;
            for (int i = 0; i < 11; i++) {
                Integer v = map.get("k" + i);
                if (v == null || v.intValue() != i) return 0;
            }
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    /** Forces resize-path: 64-entry initial capacity → grow past 16. */
    public static int testChmResizePath() {
        try {
            ConcurrentHashMap<Integer, Integer> map = new ConcurrentHashMap<>();
            for (int i = 0; i < 64; i++) map.put(i, i * 2);
            if (map.size() != 64) return 0;
            for (int i = 0; i < 64; i++) {
                Integer v = map.get(i);
                if (v == null || v.intValue() != i * 2) return 0;
            }
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    /** putIfAbsent + replace + remove cycle. */
    public static int testChmMutationCycle() {
        try {
            ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
            // Initial put.
            Integer prev1 = map.putIfAbsent("k", 1);
            if (prev1 != null) return 0;
            // putIfAbsent on existing key returns the existing mapping.
            Integer prev2 = map.putIfAbsent("k", 99);
            if (prev2 == null || prev2.intValue() != 1) return 0;
            // replace overwrites.
            Integer prev3 = map.replace("k", 42);
            if (prev3 == null || prev3.intValue() != 1) return 0;
            // remove returns the previous value.
            Integer prev4 = map.remove("k");
            if (prev4 == null || prev4.intValue() != 42) return 0;
            // After remove the key is gone.
            if (map.containsKey("k")) return 0;
            if (map.size() != 0) return 0;
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }

    /** clear + isEmpty + size invariants on a populated map. */
    public static int testChmClearEmpty() {
        try {
            ConcurrentHashMap<String, String> map = new ConcurrentHashMap<>();
            for (int i = 0; i < 50; i++) map.put("a" + i, "v" + i);
            if (map.isEmpty()) return 0;
            if (map.size() != 50) return 0;
            map.clear();
            if (!map.isEmpty()) return 0;
            if (map.size() != 0) return 0;
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }
}
