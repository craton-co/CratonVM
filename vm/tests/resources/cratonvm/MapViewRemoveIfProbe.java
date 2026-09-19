// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

import java.util.Hashtable;
import java.util.LinkedHashMap;
import java.util.HashMap;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.ConcurrentHashMap;
import java.util.function.Supplier;

/**
 * `removeIf` through a map's `keySet()` / `entrySet()` / `values()` view must
 * delete from the BACKING map and report whether it changed anything.
 *
 * `ConcurrentHashMap$EntrySetView` is the one carrier in this family that
 * *declares* `removeIf` (`return map.removeEntryIf(filter)`), so CratonVM's
 * receiver-has-own-bytecode rule ran the JDK body over a minted view whose
 * `map` field is null and every caller got
 * `NullPointerException: ... because "this.map" is null`. Spring's
 * `DefaultContextCache.remove` does exactly
 * `this.contextMap.entrySet().removeIf(..)` from a `@DirtiesContext`
 * `afterTestClass` callback.
 *
 * Every row checks the map, not the view, so a view that "removed" from a
 * detached snapshot fails here too.
 */
public final class MapViewRemoveIfProbe {

    private static int failures;

    private static void fail(String message) {
        failures++;
        System.out.println("MAPVIEW_REMOVEIF_FAIL " + message);
    }

    private static void expect(String tag, Object actual, Object expected) {
        if (!String.valueOf(expected).equals(String.valueOf(actual))) {
            fail(tag + ": " + actual + ", expected " + expected);
        }
    }

    private static Map<String, Integer> seed(Supplier<Map<String, Integer>> factory) {
        Map<String, Integer> m = factory.get();
        m.put("keep1", 1);
        m.put("drop1", 2);
        m.put("keep2", 3);
        m.put("drop2", 4);
        return m;
    }

    private static void checkFamily(String family, Supplier<Map<String, Integer>> factory) {
        // entrySet().removeIf — the shape that NPE'd on ConcurrentHashMap.
        Map<String, Integer> m = seed(factory);
        boolean changed = m.entrySet().removeIf(e -> e.getKey().startsWith("drop"));
        expect(family + " entrySet.removeIf returned", changed, true);
        expect(family + " entrySet.removeIf size", m.size(), 2);
        expect(family + " entrySet.removeIf kept keep1", m.containsKey("keep1"), true);
        expect(family + " entrySet.removeIf kept keep2", m.containsKey("keep2"), true);
        expect(family + " entrySet.removeIf dropped drop1", m.containsKey("drop1"), false);
        expect(family + " entrySet.removeIf dropped drop2", m.containsKey("drop2"), false);

        // A predicate that matches nothing must report false and change nothing.
        Map<String, Integer> untouched = seed(factory);
        boolean none = untouched.entrySet().removeIf(e -> false);
        expect(family + " entrySet.removeIf(false) returned", none, false);
        expect(family + " entrySet.removeIf(false) size", untouched.size(), 4);

        // keySet().removeIf
        Map<String, Integer> byKey = seed(factory);
        boolean keyChanged = byKey.keySet().removeIf(k -> k.startsWith("drop"));
        expect(family + " keySet.removeIf returned", keyChanged, true);
        expect(family + " keySet.removeIf size", byKey.size(), 2);
        expect(family + " keySet.removeIf dropped drop1", byKey.containsKey("drop1"), false);

        // values().removeIf
        Map<String, Integer> byValue = seed(factory);
        boolean valChanged = byValue.values().removeIf(v -> v % 2 == 0);
        expect(family + " values.removeIf returned", valChanged, true);
        expect(family + " values.removeIf size", byValue.size(), 2);
        expect(family + " values.removeIf dropped drop1", byValue.containsKey("drop1"), false);
        expect(family + " values.removeIf kept keep1", byValue.containsKey("keep1"), true);
    }

    public static void main(String[] args) {
        checkFamily("ConcurrentHashMap", ConcurrentHashMap::new);
        checkFamily("HashMap", HashMap::new);
        checkFamily("LinkedHashMap", LinkedHashMap::new);
        checkFamily("TreeMap", TreeMap::new);
        checkFamily("Hashtable", Hashtable::new);

        System.out.println("failures=" + failures);
        if (failures == 0) {
            System.out.println("MAPVIEW_REMOVEIF_OK");
        }
    }

    private MapViewRemoveIfProbe() {}
}
