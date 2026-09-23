// JAVA21+
package cratonvm;

import java.util.concurrent.ConcurrentHashMap;

/**
 * WP4.6 — ConcurrentHashMap regression probe (cratonvm side).
 *
 * Verifies the K1-family fix at vm/src/runtime/value_stack.rs:385-460
 * (Value::Object(None)/Uninitialized → 0 in pop_int/pop_long) does not
 * regress under CHM's primitive-field-driven CAS retries.
 *
 * Most methods return 1 on success and 0 on failure. The no-resize boxed-value
 * probes return negative stage codes for assertion misses and rethrow unexpected
 * VM/linkage errors so the Rust harness can report the Java exception class.
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
     * working subset of CHM put/get on cratonvm even when the larger
     * post-resize path regresses.
     */
    public static int testChmPreResizePutGet() throws Throwable {
        try {
            ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
            for (int i = 0; i < 11; i++) {
                map.put("k" + i, i);
            }
            if (map.size() != 11) return -1;
            for (int i = 0; i < 11; i++) {
                Integer v = map.get("k" + i);
                if (v == null) return -100 - i;
                if (v.intValue() != i) return -200 - i;
            }
            return 1;
        } catch (NullPointerException t) {
            return -991;
        } catch (ClassCastException t) {
            return -992;
        } catch (ArrayIndexOutOfBoundsException t) {
            return -993;
        } catch (IllegalStateException t) {
            return -994;
        } catch (Throwable t) {
            throw t;
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
    public static int testChmMutationCycle() throws Throwable {
        try {
            ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
            // Initial put.
            Integer prev1 = map.putIfAbsent("k", 1);
            if (prev1 != null) return -1;
            // putIfAbsent on existing key returns the existing mapping.
            Integer prev2 = map.putIfAbsent("k", 99);
            if (prev2 == null) return -2;
            if (prev2.intValue() != 1) return -3;
            // replace overwrites.
            Integer prev3 = map.replace("k", 42);
            if (prev3 == null) return -4;
            if (prev3.intValue() != 1) return -5;
            // remove returns the previous value.
            Integer prev4 = map.remove("k");
            if (prev4 == null) return -6;
            if (prev4.intValue() != 42) return -7;
            // After remove the key is gone.
            if (map.containsKey("k")) return -8;
            if (map.size() != 0) return -9;
            return 1;
        } catch (NullPointerException t) {
            return -991;
        } catch (ClassCastException t) {
            return -992;
        } catch (ArrayIndexOutOfBoundsException t) {
            return -993;
        } catch (IllegalStateException t) {
            return -994;
        } catch (Throwable t) {
            throw t;
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
